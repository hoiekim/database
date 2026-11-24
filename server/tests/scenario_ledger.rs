//! A personal-finance ledger, said through the client: users and sharing
//! groups, accounts, transactions with exact decimal amounts, categories,
//! recurring charges, snapshots, soft deletes and delta sync. The suite
//! covers the schema and its migration, a generic builder that most
//! statements go through, and the statements written by hand.
//!
//! Three things a SQL database does for such a ledger are done here by the
//! test file, as an application has to: what would be a SQL function,
//! `sharing_user_ids`, is a Rust function that returns an expression; the
//! builder's `SELECT *` and `RETURNING *` name the model's columns; and the
//! caller chooses a statement's way into a table (the index and the range).
//!
//! Where the client has no word for what a statement says in SQL, the
//! nearest thing stands in and the helper says so: no regular expressions
//! (`highest_unknown`), no query without a table (`sharing_group`,
//! `suggest_pair`), no CTE and no `UNION` (`suggest`, `accept_invitation`),
//! no unnesting of parallel arrays (`create_default_splits`).
//!
//! Where `docs/CLIENT.md` is silent the tests expect what Postgres does:
//! `date - date` is a number of days and `date + n` a date; a key that is
//! NULL, and an empty list of keys, find no row; a key of the wrong kind is
//! a `TypeMismatch`; `lower()` folds letters beyond ASCII; an upsert's
//! assignments and its condition read the row as it was, and the proposed
//! row through `excluded`.

mod common;
use common::*;

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde_json::json;

/// One row of a builder write: the columns the caller supplied, in order.
/// A column left out is not touched (an insert gives it its default); one
/// present with `Value::Null` is written as NULL.
type Data = Vec<(&'static str, Value)>;

macro_rules! data {
    ($($column:literal: $value:expr),* $(,)?) => {
        vec![$(($column, ::std::convert::Into::<Value>::into($value))),*]
    };
}

fn req(name: &str, ty: Type) -> ColumnDef {
    ColumnDef::new(name, ty)
}

fn opt(name: &str, ty: Type) -> ColumnDef {
    ColumnDef::new(name, ty).nullable()
}

/// `NUMERIC(15,2)`: amounts and balances.
fn money() -> Type {
    Type::decimal(15, 2)
}

/// `NUMERIC(15,6)`: prices, quantities and index values.
fn price() -> Type {
    Type::decimal(15, 6)
}

fn texts() -> Type {
    Type::list(Type::Text)
}

/// The change feed's clock: every builder write stamps it again.
fn updated() -> ColumnDef {
    opt("updated", Type::Timestamp).default_now()
}

/// Nullable on purpose: the live predicate treats NULL as live.
fn is_deleted() -> ColumnDef {
    opt("is_deleted", Type::Bool).default(false)
}

fn idx(name: &str, columns: &[&str]) -> IndexDef {
    IndexDef::new(name).on(columns)
}

/// Unique among the users that have an email, whatever its letter case.
fn lower_email_index() -> IndexDef {
    IndexDef::new("idx_users_lower_email").on_expr(col("email").lower()).unique().filter(col("email").is_not_null())
}

/// The ledger's models, in the order they are created: a table comes after
/// the ones it references. The plain indexes are the ones the statements
/// here read through, since nothing plans them: where Postgres would take
/// an index on `account_id` alone and sort by date afterwards, this has
/// `(account_id, date)`.
fn models() -> &'static [TableDef] {
    static MODELS: OnceLock<Vec<TableDef>> = OnceLock::new();
    MODELS.get_or_init(|| {
        vec![
            TableDef::new("users")
                .column(req("user_id", Type::Uuid).default_random_uuid())
                .column(req("username", Type::Text))
                .column(opt("password", Type::Text))
                .column(opt("email", Type::Text))
                .column(opt("expiry", Type::Timestamp))
                .column(opt("token", Type::Text))
                .column(opt("auth_sub", Type::Text))
                .column(updated())
                .column(is_deleted())
                .primary_key(["user_id"])
                .unique("users_username_key", ["username"])
                .unique("users_auth_sub_key", ["auth_sub"])
                .index(lower_email_index())
                .check("username_length", col("username").length().le(255)),
            TableDef::new("sessions")
                .column(req("session_id", Type::Text))
                .column(opt("user_user_id", Type::Uuid))
                .column(opt("user_username", Type::Text))
                .column(opt("cookie_original_max_age", Type::Int))
                .column(opt("cookie_max_age", Type::Int))
                .column(opt("cookie_signed", Type::Bool))
                .column(opt("cookie_expires", Type::Timestamp))
                .column(opt("cookie_http_only", Type::Bool))
                .column(opt("cookie_path", Type::Text))
                .column(opt("cookie_domain", Type::Text))
                .column(opt("cookie_secure", Type::Bool))
                .column(opt("cookie_same_site", Type::Text))
                .column(opt("created_at", Type::Timestamp).default_now())
                .column(updated())
                .column(opt("auth_sid", Type::Text))
                .column(opt("auth_refresh_token", Type::Text))
                .column(opt("auth_recheck_at", Type::Timestamp))
                .primary_key(["session_id"])
                .index(idx("idx_sessions_auth_sid", &["auth_sid"]))
                .index(idx("idx_sessions_user_user_id", &["user_user_id"])),
            TableDef::new("institutions")
                .column(req("institution_id", Type::Text))
                .column(opt("name", Type::Text))
                .column(opt("raw", Type::Json))
                .column(updated())
                .primary_key(["institution_id"]),
            TableDef::new("securities")
                .column(req("security_id", Type::Text))
                .column(opt("name", Type::Text))
                .column(opt("ticker_symbol", Type::Text))
                .column(opt("type", Type::Text))
                .column(opt("close_price", price()))
                .column(opt("close_price_as_of", Type::Date))
                .column(opt("iso_currency_code", Type::Text))
                .column(opt("isin", Type::Text))
                .column(opt("cusip", Type::Text))
                .column(opt("raw", Type::Json))
                .column(updated())
                .primary_key(["security_id"])
                .index(IndexDef::new("idx_securities_ticker_symbol_updated").on(["ticker_symbol"]).on_desc("updated")),
            TableDef::new("items")
                .column(req("item_id", Type::Text))
                .column(req("user_id", Type::Uuid))
                .column(opt("access_token", Type::Text))
                .column(opt("institution_id", Type::Text))
                .column(opt("available_products", texts()))
                .column(opt("cursor", Type::Text))
                .column(opt("status", Type::Text))
                .column(opt("status_reason", Type::Text))
                .column(opt("provider", Type::Text))
                .column(opt("last_sync_status", Type::Text))
                .column(opt("last_sync_at", Type::Timestamp))
                .column(opt("last_sync_error", Type::Text))
                .column(opt("raw", Type::Json))
                .column(updated())
                .column(is_deleted())
                .primary_key(["item_id"])
                .index(idx("idx_items_user_id", &["user_id"]))
                .index(idx("idx_items_institution_id", &["institution_id"]))
                .foreign_key(["user_id"], "users", OnDelete::Restrict),
            TableDef::new("accounts")
                .column(req("account_id", Type::Text))
                .column(req("user_id", Type::Uuid))
                .column(req("item_id", Type::Text))
                .column(req("institution_id", Type::Text))
                .column(opt("name", Type::Text))
                .column(opt("type", Type::Text))
                .column(opt("subtype", Type::Text))
                .column(opt("balances_available", money()))
                .column(opt("balances_current", money()))
                .column(opt("balances_limit", money()))
                .column(opt("balances_iso_currency_code", Type::Text))
                .column(opt("custom_name", Type::Text))
                .column(opt("hide", Type::Bool).default(false))
                .column(opt("archived", Type::Bool).default(false))
                .column(opt("label_budget_id", Type::Uuid))
                .column(opt("graph_options_use_snapshots", Type::Bool).default(true))
                .column(opt("graph_options_use_holding_snapshots", Type::Bool).default(true))
                .column(opt("graph_options_use_transactions", Type::Bool).default(true))
                .column(opt("plan_fund_mappings", Type::Json))
                .column(opt("raw", Type::Json))
                .column(updated())
                .column(is_deleted())
                .primary_key(["account_id"])
                .index(idx("idx_accounts_user_id", &["user_id"]))
                .index(idx("idx_accounts_item_id", &["item_id"]))
                .index(idx("idx_accounts_institution_id", &["institution_id"]))
                .foreign_key(["user_id"], "users", OnDelete::Restrict),
            TableDef::new("holdings")
                .column(req("holding_id", Type::Text))
                .column(req("user_id", Type::Uuid))
                .column(req("account_id", Type::Text))
                .column(req("security_id", Type::Text))
                .column(opt("institution_price", price()))
                .column(opt("institution_price_as_of", Type::Date))
                .column(opt("institution_value", money()))
                .column(opt("cost_basis", money()))
                .column(opt("quantity", price()))
                .column(opt("iso_currency_code", Type::Text))
                .column(opt("raw", Type::Json))
                .column(updated())
                .column(is_deleted())
                .primary_key(["holding_id"])
                .index(idx("idx_holdings_user_id", &["user_id"]))
                .index(idx("idx_holdings_account_id", &["account_id"]))
                .index(idx("idx_holdings_security_id", &["security_id"]))
                .foreign_key(["user_id"], "users", OnDelete::Restrict),
            TableDef::new("transactions")
                .column(req("transaction_id", Type::Text))
                .column(req("user_id", Type::Uuid))
                .column(req("account_id", Type::Text))
                .column(opt("name", Type::Text))
                .column(opt("merchant_name", Type::Text))
                .column(opt("amount", money()))
                .column(opt("iso_currency_code", Type::Text))
                .column(req("date", Type::Date))
                .column(opt("pending", Type::Bool).default(false))
                .column(opt("pending_transaction_id", Type::Text))
                .column(opt("payment_channel", Type::Text))
                .column(opt("location_country", Type::Text))
                .column(opt("location_region", Type::Text))
                .column(opt("location_city", Type::Text))
                .column(opt("label_budget_id", Type::Uuid))
                .column(opt("label_category_id", Type::Uuid))
                .column(opt("label_memo", Type::Text))
                .column(opt("label_category_confidence", Type::Float))
                .column(opt("raw", Type::Json))
                .column(updated())
                .column(is_deleted())
                .column(req("source", Type::Text).default("bank"))
                .primary_key(["transaction_id"])
                .index(idx("idx_transactions_account_id_date", &["account_id", "date"]))
                .index(idx("idx_transactions_user_id_date", &["user_id", "date"]))
                .index(idx("idx_transactions_user_id_transaction_id", &["user_id", "transaction_id"]))
                .index(idx("idx_transactions_user_id_updated", &["user_id", "updated"]))
                .index(idx("idx_transactions_user_id_amount_date", &["user_id", "amount", "date"]))
                .foreign_key(["user_id"], "users", OnDelete::Restrict)
                .check("iso_currency_code_length", col("iso_currency_code").length().le(10)),
            TableDef::new("transaction_pairs")
                .column(req("pair_id", Type::Uuid))
                .column(req("user_id", Type::Uuid))
                .column(req("transaction_id_a", Type::Text))
                .column(req("transaction_id_b", Type::Text))
                .column(req("status", Type::Text))
                .column(opt("created_at", Type::Timestamp).default_now())
                .column(updated())
                .column(is_deleted())
                .primary_key(["pair_id"])
                .unique("transaction_pairs_pair_unique", ["transaction_id_a", "transaction_id_b"])
                .index(idx("idx_transaction_pairs_transaction_id_a", &["transaction_id_a"]))
                .index(idx("idx_transaction_pairs_transaction_id_b", &["transaction_id_b"]))
                .index(idx("idx_transaction_pairs_user_id_updated", &["user_id", "updated"]))
                .foreign_key(["transaction_id_a"], "transactions", OnDelete::Cascade)
                .foreign_key(["transaction_id_b"], "transactions", OnDelete::Cascade)
                .foreign_key(["user_id"], "users", OnDelete::Restrict),
            TableDef::new("recurrings")
                .column(req("recurring_id", Type::Uuid))
                .column(req("user_id", Type::Uuid))
                .column(req("merchant_key", Type::Text))
                .column(req("merchant_name", Type::Text))
                .column(req("cadence", Type::Text))
                .column(req("is_income", Type::Bool))
                .column(req("status", Type::Text))
                .column(req("transaction_ids", texts()).default(Vec::<String>::new()))
                .column(opt("label_budget_id", Type::Uuid))
                .column(opt("label_category_id", Type::Uuid))
                .column(req("splits", Type::Json).default(json!([])))
                .column(opt("splits_since", Type::Date))
                .column(opt("created_at", Type::Timestamp).default_now())
                .column(updated())
                .column(is_deleted())
                .primary_key(["recurring_id"])
                .unique("recurrings_user_merchant_direction_unique", ["user_id", "merchant_key", "is_income"])
                .index(idx("idx_recurrings_user_id_updated", &["user_id", "updated"]))
                .foreign_key(["user_id"], "users", OnDelete::Restrict),
            TableDef::new("investment_transactions")
                .column(req("investment_transaction_id", Type::Text))
                .column(req("user_id", Type::Uuid))
                .column(req("account_id", Type::Text))
                .column(opt("security_id", Type::Text))
                .column(req("date", Type::Date))
                .column(opt("name", Type::Text))
                .column(opt("amount", money()))
                .column(opt("quantity", price()))
                .column(opt("price", price()))
                .column(opt("iso_currency_code", Type::Text))
                .column(opt("type", Type::Text))
                .column(opt("subtype", Type::Text))
                .column(opt("label_budget_id", Type::Uuid))
                .column(opt("label_category_id", Type::Uuid))
                .column(opt("label_memo", Type::Text))
                .column(opt("raw", Type::Json))
                .column(updated())
                .column(is_deleted())
                .column(req("source", Type::Text).default("bank"))
                .primary_key(["investment_transaction_id"])
                .index(idx("idx_investment_transactions_account_id_date", &["account_id", "date"]))
                .index(idx("idx_investment_transactions_user_id_investment_transaction_id", &["user_id", "investment_transaction_id"]))
                .index(idx("idx_investment_transactions_user_id_updated", &["user_id", "updated"]))
                .foreign_key(["user_id"], "users", OnDelete::Restrict),
            TableDef::new("split_transactions")
                .column(req("split_transaction_id", Type::Uuid).default_random_uuid())
                .column(req("user_id", Type::Uuid))
                .column(req("transaction_id", Type::Text))
                .column(req("account_id", Type::Text))
                .column(opt("amount", money()).default(0))
                .column(req("date", Type::Date))
                .column(opt("custom_name", Type::Text).default(""))
                .column(opt("label_budget_id", Type::Uuid))
                .column(opt("label_category_id", Type::Uuid))
                .column(opt("label_memo", Type::Text))
                .column(opt("label_category_confidence", Type::Float))
                .column(updated())
                .column(is_deleted())
                .primary_key(["split_transaction_id"])
                .index(idx("idx_split_transactions_transaction_id", &["transaction_id"]))
                .index(idx("idx_split_transactions_account_id", &["account_id"]))
                .index(idx("idx_split_transactions_user_id_updated", &["user_id", "updated"]))
                .foreign_key(["user_id"], "users", OnDelete::Restrict),
            TableDef::new("budgets")
                .column(req("budget_id", Type::Uuid).default_random_uuid())
                .column(req("user_id", Type::Uuid))
                .column(opt("name", Type::Text).default("Unnamed"))
                .column(opt("iso_currency_code", Type::Text).default("USD"))
                .column(opt("roll_over", Type::Bool).default(false))
                .column(opt("roll_over_start_date", Type::Date))
                .column(opt("capacities", Type::Json))
                .column(updated())
                .column(is_deleted())
                .primary_key(["budget_id"])
                .index(idx("idx_budgets_user_id", &["user_id"]))
                .foreign_key(["user_id"], "users", OnDelete::Restrict),
            TableDef::new("sections")
                .column(req("section_id", Type::Uuid).default_random_uuid())
                .column(req("user_id", Type::Uuid))
                .column(req("budget_id", Type::Uuid))
                .column(opt("name", Type::Text).default("Unnamed"))
                .column(opt("roll_over", Type::Bool).default(false))
                .column(opt("roll_over_start_date", Type::Date))
                .column(opt("capacities", Type::Json))
                .column(updated())
                .column(is_deleted())
                .primary_key(["section_id"])
                .index(idx("idx_sections_user_id", &["user_id"]))
                .index(idx("idx_sections_budget_id", &["budget_id"]))
                .foreign_key(["budget_id"], "budgets", OnDelete::Restrict)
                .foreign_key(["user_id"], "users", OnDelete::Restrict),
            TableDef::new("categories")
                .column(req("category_id", Type::Uuid).default_random_uuid())
                .column(req("user_id", Type::Uuid))
                .column(req("section_id", Type::Uuid))
                .column(opt("name", Type::Text).default("Unnamed"))
                .column(opt("roll_over", Type::Bool).default(false))
                .column(opt("roll_over_start_date", Type::Date))
                .column(opt("capacities", Type::Json))
                .column(updated())
                .column(is_deleted())
                .primary_key(["category_id"])
                .index(idx("idx_categories_user_id", &["user_id"]))
                .index(idx("idx_categories_section_id", &["section_id"]))
                .foreign_key(["section_id"], "sections", OnDelete::Restrict)
                .foreign_key(["user_id"], "users", OnDelete::Restrict),
            TableDef::new("snapshots")
                .column(req("snapshot_id", Type::Text))
                .column(opt("user_id", Type::Uuid))
                .column(req("snapshot_date", Type::Timestamp))
                .column(req("snapshot_type", Type::Text))
                .column(opt("account_id", Type::Text))
                .column(opt("balances_available", money()))
                .column(opt("balances_current", money()))
                .column(opt("balances_limit", money()))
                .column(opt("balances_iso_currency_code", Type::Text))
                .column(opt("security_id", Type::Text))
                .column(opt("close_price", price()))
                .column(opt("holding_account_id", Type::Text))
                .column(opt("holding_security_id", Type::Text))
                .column(opt("institution_price", price()))
                .column(opt("institution_value", money()))
                .column(opt("cost_basis", money()))
                .column(opt("quantity", price()))
                .column(updated())
                .column(is_deleted())
                .primary_key(["snapshot_id"])
                .index(idx("idx_snapshots_user_id_snapshot_id", &["user_id", "snapshot_id"]))
                .index(idx("idx_snapshots_user_id_updated", &["user_id", "updated"]))
                .index(idx("idx_snapshots_user_id_snapshot_date", &["user_id", "snapshot_date", "snapshot_id"]))
                .index(idx("idx_snapshots_account_id_snapshot_date", &["account_id", "snapshot_date"]))
                .index(idx("idx_snapshots_security_id_snapshot_date", &["security_id", "snapshot_date"]))
                .index(idx("idx_snapshots_holding_account_id", &["holding_account_id"]))
                .index(idx("idx_snapshots_snapshot_type_snapshot_id", &["snapshot_type", "snapshot_id"])),
            TableDef::new("charts")
                .column(req("chart_id", Type::Uuid).default_random_uuid())
                .column(opt("user_id", Type::Uuid))
                .column(opt("name", Type::Text).default("Unnamed"))
                .column(opt("type", Type::Text))
                .column(opt("configuration", Type::Json))
                .column(updated())
                .column(is_deleted())
                .primary_key(["chart_id"])
                .index(idx("idx_charts_user_id", &["user_id"]))
                .foreign_key(["user_id"], "users", OnDelete::Restrict),
            TableDef::new("api_keys")
                .column(req("key_id", Type::Uuid).default_random_uuid())
                .column(req("user_id", Type::Uuid))
                .column(req("name", Type::Text))
                .column(req("key_hash", Type::Text))
                .column(req("key_prefix", Type::Text))
                .column(req("scopes", texts()).default(Vec::<String>::new()))
                .column(req("created_at", Type::Timestamp).default_now())
                .column(opt("last_used_at", Type::Timestamp))
                .column(opt("revoked_at", Type::Timestamp))
                .column(opt("expires_at", Type::Timestamp))
                .column(updated())
                .primary_key(["key_id"])
                .unique("api_keys_key_hash_key", ["key_hash"])
                .index(idx("idx_api_keys_user_id_created_at", &["user_id", "created_at"]))
                .foreign_key(["user_id"], "users", OnDelete::Cascade),
            TableDef::new("rejected_categories")
                .column(req("transaction_id", Type::Text))
                .column(req("user_id", Type::Uuid))
                .column(req("category_id", Type::Uuid))
                .column(opt("rejected_at", Type::Timestamp).default_now())
                .primary_key(["transaction_id", "category_id"])
                .index(idx("idx_rejected_categories_user_id", &["user_id"]))
                .foreign_key(["category_id"], "categories", OnDelete::Cascade)
                .foreign_key(["transaction_id"], "transactions", OnDelete::Cascade)
                .foreign_key(["user_id"], "users", OnDelete::Cascade),
            TableDef::new("cpi_snapshots")
                .column(req("cpi_snapshot_id", Type::Text))
                .column(req("series_id", Type::Text))
                .column(req("snapshot_date", Type::Date))
                .column(req("index_value", price()))
                .column(updated())
                .primary_key(["cpi_snapshot_id"])
                .index(idx("idx_cpi_snapshots_series_id_snapshot_date", &["series_id", "snapshot_date"])),
            TableDef::new("sharing_members")
                .column(req("user_id", Type::Uuid))
                .column(req("group_id", Type::Uuid))
                .column(updated())
                .primary_key(["user_id"])
                .index(idx("idx_sharing_members_group_id", &["group_id"]))
                .foreign_key(["user_id"], "users", OnDelete::Cascade),
            TableDef::new("sharing_invitations")
                .column(req("invitation_id", Type::Uuid).default_random_uuid())
                .column(req("inviter_user_id", Type::Uuid))
                .column(req("invitee_user_id", Type::Uuid))
                .column(updated())
                .primary_key(["invitation_id"])
                .unique("sharing_invitations_pair_unique", ["inviter_user_id", "invitee_user_id"])
                .index(idx("idx_sharing_invitations_invitee_user_id", &["invitee_user_id"]))
                .foreign_key(["invitee_user_id"], "users", OnDelete::Cascade)
                .foreign_key(["inviter_user_id"], "users", OnDelete::Cascade),
            TableDef::new("sharing_email_invitations")
                .column(req("invitation_id", Type::Uuid).default_random_uuid())
                .column(req("inviter_user_id", Type::Uuid))
                .column(req("invitee_email", Type::Text))
                .column(req("expires_at", Type::Timestamp))
                .column(updated())
                .primary_key(["invitation_id"])
                .unique("sharing_email_invitations_pair_unique", ["inviter_user_id", "invitee_email"])
                .index(idx("idx_sharing_email_invitations_invitee_email", &["invitee_email"]))
                .foreign_key(["inviter_user_id"], "users", OnDelete::Cascade),
        ]
    })
}

/// What the application does at every start: each model's table is created
/// unless it is there, one statement per table.
async fn schema(db: &Client) -> DbResult<()> {
    for model in models() {
        db.create_table_if_not_exists(model.clone()).await?;
    }
    Ok(())
}

fn model(table: &str) -> &'static TableDef {
    models().iter().find(|m| m.name == table).expect("one of the ledger's tables")
}

/// The single key column the generic operations work on. They refuse a
/// table with a composite key.
fn primary_key(table: &str) -> &'static str {
    let key = &model(table).primary_key;
    assert_eq!(key.len(), 1, "{table} has a composite primary key");
    &key[0]
}

/// `SELECT *` and `RETURNING *`: the model's columns, in its order.
fn all_columns(table: &str) -> Vec<&'static str> {
    model(table).columns.iter().map(|c| c.name.as_str()).collect()
}

fn soft_deletes(table: &str) -> bool {
    model(table).column_index("is_deleted").is_some()
}

fn column_names(table: &TableDef) -> Vec<&str> {
    table.columns.iter().map(|c| c.name.as_str()).collect()
}

/// `LIVE`: `(is_deleted IS NULL OR is_deleted = FALSE)`.
fn live() -> Expr {
    col("is_deleted").is_null().or(col("is_deleted").eq(false))
}

/// `LIVE` for one table of a join.
fn live_in(alias: &str) -> Expr {
    let flag = format!("{alias}.is_deleted");
    col(&flag).is_null().or(col(&flag).eq(false))
}

/// In SQL, a function `sharing_user_ids(viewer)`: every other member of the
/// viewer's sharing group, then the viewer. For someone in no group, just
/// the viewer. It is part of the statement it appears in, so membership is
/// read afresh by every statement.
fn sharing_user_ids(viewer: Uuid) -> Expr {
    let peers = Query::table("sharing_members")
        .alias("own")
        .key([viewer])
        .join(
            Query::table("sharing_members")
                .alias("peer")
                .index("idx_sharing_members_group_id", KeyRange::eq([col("own.group_id")]))
                .filter(col("peer.user_id").ne(viewer)),
        )
        .select("user_id", col("peer.user_id"));
    list_of(peers).list_concat(list([val(viewer)]))
}

/// `SELECT unnest(sharing_user_ids($1))`: the group as rows, for the
/// statements that want its members as ranges of an index. There is no
/// query without a table, so the viewer's own row carries the list.
async fn sharing_group(db: &impl Db, viewer: Uuid) -> DbResult<Vec<Uuid>> {
    db.query(Query::table("users").key([viewer]).each("member", sharing_user_ids(viewer)).select("user_id", col("member"))).await?.column()
}

/// `$1::uuid[]`: a group read once and bound as a list.
fn uuid_list(ids: &[Uuid]) -> Expr {
    val(ids.iter().map(|id| id.0).collect::<Vec<[u8; 16]>>())
}

/// Whose rows a statement may see or change.
#[derive(Clone, Copy)]
enum Scope {
    /// Global tables, and jobs that walk every user.
    Everyone,
    /// `user_id = $n`.
    Owner(Uuid),
    /// `user_id = ANY(sharing_user_ids($n))`.
    Group(Uuid),
}

impl Scope {
    fn on(self, column: &str) -> Option<Expr> {
        match self {
            Scope::Everyone => None,
            Scope::Owner(user) => Some(col(column).eq(user)),
            Scope::Group(viewer) => Some(col(column).in_list(sharing_user_ids(viewer))),
        }
    }
}

/// What a generated `WHERE` can hold, ANDed together.
#[derive(Clone)]
struct Filters {
    scope: Scope,
    conditions: Vec<Expr>,
    tombstones: bool,
}

impl Filters {
    fn new(scope: Scope) -> Filters {
        Filters { scope, conditions: Vec::new(), tombstones: false }
    }

    /// `col = $n`, or `col IS NULL` when the value is null.
    fn eq(mut self, column: &str, value: impl Into<Value>) -> Filters {
        let value = value.into();
        self.conditions.push(if value.is_null() { col(column).is_null() } else { col(column).eq(value) });
        self
    }

    /// `col IN ($a, $b, ..)`. An empty list adds nothing: it does not
    /// mean "match nothing".
    fn any_of<V: Into<Value>>(mut self, column: &str, values: impl IntoIterator<Item = V>) -> Filters {
        let values: Vec<Value> = values.into_iter().map(Into::into).collect();
        if !values.is_empty() {
            self.conditions.push(col(column).in_list(values));
        }
        self
    }

    /// `lower(col) = lower($n)`, both sides folded by the server.
    fn ignoring_case(mut self, column: &str, text: &str) -> Filters {
        self.conditions.push(col(column).lower().eq(val(text).lower()));
        self
    }

    /// `col >= $n`: the lower end of a date range, inclusive.
    fn since(mut self, column: &str, bound: impl Into<Value>) -> Filters {
        self.conditions.push(col(column).ge(bound.into()));
        self
    }

    /// `col <= $n`: the upper end, inclusive.
    fn until(mut self, column: &str, bound: impl Into<Value>) -> Filters {
        self.conditions.push(col(column).le(bound.into()));
        self
    }

    /// Leaves `LIVE` out, so soft-deleted rows come back as tombstones.
    fn with_tombstones(mut self) -> Filters {
        self.tombstones = true;
        self
    }
}

/// The builder's query: the rows `from` reads (a whole table, or the index
/// range the caller chose) that pass the filters. `LIVE` is added for a
/// soft-deletable table unless tombstones were asked for.
fn find(from: Query, filters: &Filters) -> Query {
    let mut query = from;
    if let Some(scope) = filters.scope.on("user_id") {
        query = query.filter(scope);
    }
    for condition in &filters.conditions {
        query = query.filter(condition.clone());
    }
    if soft_deletes(&query.table) && !filters.tombstones {
        query = query.filter(live());
    }
    query
}

/// The builder's query for one row: the first row or none, with no
/// order asked for.
async fn find_one(db: &impl Db, from: Query, filters: &Filters) -> DbResult<Option<Row>> {
    db.query_opt(find(from, filters).limit(1)).await
}

/// How many rows a stream reads per statement.
const BATCH: u64 = 500;

/// The builder's stream: keyset pages on the primary key, one statement
/// each, until a page comes back short. The pages are returned as read, so
/// an exact multiple ends with an empty one. Nothing ties the statements to
/// one moment, as nothing does in Postgres.
async fn find_in_batches(db: &impl Db, table: &str, filters: &Filters, batch: u64) -> DbResult<Vec<Vec<Row>>> {
    let key = primary_key(table);
    let mut pages = Vec::new();
    let mut after: Option<Value> = None;
    loop {
        let range = match after.take() {
            Some(last) => KeyRange::new(Bound::excluded([last]), Bound::Unbounded),
            None => KeyRange::all(),
        };
        let page = db.query(find(Query::table(table).range(range), filters).limit(batch)).await?.into_vec();
        let short = (page.len() as u64) < batch;
        if let Some(last) = page.last() {
            after = Some(last.get(key)?);
        }
        pages.push(page);
        if short {
            return Ok(pages);
        }
    }
}

/// The builder's query by ids: rows by primary key; ids that are missing,
/// deleted or out of scope are simply absent. No statement for no ids.
async fn find_by_ids(db: &impl Db, table: &str, ids: Vec<Value>, scope: Scope) -> DbResult<Vec<Row>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(db.query(find(Query::table(table).keys(ids.into_iter().map(|id| [id])), &Filters::new(scope))).await?.into_vec())
}

/// What a builder write returns: `["*"]` for the whole row, nothing for
/// just the primary key.
fn returned(table: &str, columns: &[&'static str]) -> Vec<&'static str> {
    match columns {
        [] => vec![primary_key(table)],
        ["*"] => all_columns(table),
        named => named.to_vec(),
    }
}

/// The builder's insert: `updated` first, then the supplied columns; the
/// rest take their defaults. With `do_nothing`, a row that breaks any
/// unique index is skipped and nothing comes back for it.
async fn insert(db: &impl Db, table: &str, data: Data, returning: &[&'static str], do_nothing: bool) -> DbResult<Rows> {
    let mut insert = Insert::into(table).set("updated", now());
    for (column, value) in data {
        insert = insert.set(column, value);
    }
    if do_nothing {
        insert = insert.on_conflict_do_nothing();
    }
    db.insert(insert.returning(returned(table, returning))).await
}

/// The builder's update: sets the supplied columns and `updated` on the row
/// with this key, if it also passes the scope, the guards and (for a
/// soft-deletable table) `LIVE`. `raw` is never written by an update. With
/// nothing left to set no statement is sent, which is `None`.
async fn update(db: &impl Db, table: &str, id: impl Into<Value>, data: Data, scope: Scope, guards: Vec<Expr>) -> DbResult<Option<Rows>> {
    let key = primary_key(table);
    let data: Data = data.into_iter().filter(|(column, _)| *column != "raw" && *column != key).collect();
    if data.is_empty() {
        return Ok(None);
    }
    let mut update = Update::table(table).key([id.into()]).set("updated", now());
    for (column, value) in data {
        update = update.set(column, value);
    }
    if let Some(scope) = scope.on("user_id") {
        update = update.filter(scope);
    }
    for guard in guards {
        update = update.filter(guard);
    }
    if soft_deletes(table) {
        update = update.filter(live());
    }
    Ok(Some(db.update(update.returning([key])).await?))
}

/// The status a caller reports for a row, from how many rows its
/// statement touched.
fn status(rows: &Rows) -> u16 {
    if rows.affected() > 0 {
        200
    } else {
        404
    }
}

/// The same for an update, where a statement that found no row and a
/// statement that was never sent both read as "not modified".
fn update_status(outcome: &Option<Rows>) -> u16 {
    match outcome {
        Some(rows) if rows.affected() > 0 => 200,
        _ => 304,
    }
}

/// The builder's upsert: inserts, or on a conflict with `conflict` (the
/// primary key unless another unique index is named, with its columns)
/// rewrites the supplied columns that `allow` permits, and `updated`. The
/// key and the conflict columns are never rewritten. With nothing to
/// rewrite the conflict is left alone. Returns the whole row.
async fn upsert(db: &impl Db, table: &str, data: Data, allow: Option<&[&str]>, conflict: Option<(&str, &[&str])>) -> DbResult<Rows> {
    let key = primary_key(table);
    let (target, fixed): (&str, Vec<&str>) = match conflict {
        Some((index, columns)) => (index, columns.to_vec()),
        None => (PRIMARY, vec![key]),
    };
    let rewritten: Vec<&'static str> = data
        .iter()
        .map(|(column, _)| *column)
        .filter(|column| *column != key && !fixed.contains(column))
        .filter(|column| allow.is_none_or(|allow| allow.contains(column)))
        .collect();
    let mut insert = Insert::into(table).set("updated", now());
    for (column, value) in data {
        insert = insert.set(column, value);
    }
    insert = if rewritten.is_empty() {
        insert.on_conflict_do_nothing_on(target)
    } else {
        insert.on_conflict_update(target).update_excluded(rewritten).update("updated", now())
    };
    db.insert(insert.returning(all_columns(table))).await
}

/// The builder's multi-row upsert: rows are de-duplicated by key (the last
/// one wins, in the first one's place), grouped by the columns they supply,
/// and each group is one multi-row statement on the primary key.
async fn upsert_many(db: &impl Db, table: &str, rows: Vec<Data>, allow: Option<&[&str]>) -> DbResult<Vec<Row>> {
    let key = primary_key(table);
    let key_of = |row: &Data| row.iter().find(|(column, _)| *column == key).map(|(_, value)| value.clone());
    let mut unique: Vec<Data> = Vec::new();
    for row in rows {
        let id = key_of(&row);
        match unique.iter().position(|kept| id.is_some() && key_of(kept) == id) {
            Some(i) => unique[i] = row,
            None => unique.push(row),
        }
    }
    let mut groups: Vec<(Vec<&'static str>, Vec<Vec<Value>>)> = Vec::new();
    for row in unique {
        let (columns, values): (Vec<&'static str>, Vec<Value>) = row.into_iter().unzip();
        match groups.iter_mut().find(|(group, _)| *group == columns) {
            Some((_, rows)) => rows.push(values),
            None => groups.push((columns, vec![values])),
        }
    }
    let mut stored = Vec::new();
    for (columns, rows) in groups {
        let rewritten: Vec<&'static str> =
            columns.iter().copied().filter(|column| *column != key && allow.is_none_or(|allow| allow.contains(column))).collect();
        let mut insert = Insert::into(table).columns(std::iter::once("updated").chain(columns.iter().copied()));
        for values in rows {
            insert = insert.values(std::iter::once(now()).chain(values.into_iter().map(Expr::from)));
        }
        insert = if rewritten.is_empty() {
            insert.on_conflict_do_nothing_on(PRIMARY)
        } else {
            insert.on_conflict_update(PRIMARY).update_excluded(rewritten).update("updated", now())
        };
        stored.extend(db.insert(insert.returning(all_columns(table))).await?);
    }
    Ok(stored)
}

/// The `SET` of every soft delete: the flag, and `updated` so the change
/// feed carries the tombstone.
fn tombstone(rows: Query, scope: Scope) -> Update {
    let mut update = Update::rows(rows).set("is_deleted", true).set("updated", now());
    if let Some(scope) = scope.on("user_id") {
        update = update.filter(scope);
    }
    update
}

/// The builder's soft delete: whether a row was flagged. There is no `LIVE`
/// guard, so deleting a deleted row succeeds again and re-stamps it.
async fn soft_delete(db: &impl Db, table: &str, id: impl Into<Value>, scope: Scope) -> DbResult<bool> {
    let update = tombstone(Query::table(table).key([id.into()]), scope).returning([primary_key(table)]);
    Ok(db.update(update).await?.affected() > 0)
}

/// The builder's soft delete by keys: how many of them were flagged.
async fn bulk_soft_delete(db: &impl Db, table: &str, ids: Vec<Value>, scope: Scope) -> DbResult<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let update = tombstone(Query::table(table).keys(ids.into_iter().map(|id| [id])), scope).returning([primary_key(table)]);
    Ok(db.update(update).await?.affected())
}

/// The builder's soft delete by a column: `col = $1`, or `col = ANY($1)`
/// when the value is a list. An empty list sends no statement.
async fn bulk_soft_delete_by_column(db: &impl Db, table: &str, column: &str, value: impl Into<Value>, scope: Scope) -> DbResult<u64> {
    let matches = match value.into() {
        Value::List(items) if items.is_empty() => return Ok(0),
        Value::List(items) => col(column).in_list(items),
        one => col(column).eq(one),
    };
    let update = tombstone(Query::table(table).filter(matches), scope).returning([primary_key(table)]);
    Ok(db.update(update).await?.affected())
}

/// The builder's soft delete by a column of a related table:
/// `col IN (SELECT select FROM source WHERE matches = ANY($1))`.
async fn bulk_soft_delete_by_related_column(
    db: &impl Db,
    table: &str,
    column: &str,
    source: (&str, &str, &str),
    values: Vec<Value>,
    scope: Scope,
) -> DbResult<u64> {
    let (source, select, matches) = source;
    let related = Query::table(source).filter(col(matches).in_list(values)).select(select, col(select));
    let update = tombstone(Query::table(table).filter(col(column).in_list(list_of(related))), scope).returning([primary_key(table)]);
    Ok(db.update(update).await?.affected())
}

/// The builder's hard delete: whether a row was removed.
async fn hard_delete(db: &impl Db, table: &str, id: impl Into<Value>) -> DbResult<bool> {
    Ok(db.delete(Delete::from(table).key([id.into()]).returning([primary_key(table)])).await?.affected() > 0)
}

/// The builder's hard delete by keys: how many rows were removed.
async fn bulk_hard_delete(db: &impl Db, table: &str, ids: Vec<Value>) -> DbResult<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let rows = Query::table(table).keys(ids.into_iter().map(|id| [id]));
    Ok(db.delete(Delete::rows(rows).returning([primary_key(table)])).await?.affected())
}

/// The builder's hard delete by a column: `col = $1`.
async fn hard_delete_by_column(db: &impl Db, table: &str, column: &str, value: impl Into<Value>) -> DbResult<u64> {
    delete_by_condition(db, table, column, CmpOp::Eq, value).await
}

/// The builder's hard delete by a condition: `col <op> $1`.
async fn delete_by_condition(db: &impl Db, table: &str, column: &str, op: CmpOp, value: impl Into<Value>) -> DbResult<u64> {
    let condition = Expr::Cmp(op, Box::new(col(column)), Box::new(val(value.into())));
    Ok(db.delete(Delete::from(table).filter(condition).returning([primary_key(table)])).await?.affected())
}

/// A user as password sign-up makes one: the id comes from the column's
/// default and is read from what the insert returns.
async fn user(db: &impl Db, username: &str) -> DbResult<Uuid> {
    insert(db, "users", data! {"username": username}, &[], false).await?.one()?.get("user_id")
}

/// Puts users in one sharing group, as accepting an invitation does.
async fn share(db: &impl Db, group: Uuid, members: &[Uuid]) -> DbResult<()> {
    let rows = members.iter().map(|member| data! {"user_id": *member, "group_id": group}).collect();
    upsert_many(db, "sharing_members", rows, Some(&[])).await?;
    Ok(())
}

/// Alice and Bob share their data; Carol shares with nobody.
struct Household {
    alice: Uuid,
    bob: Uuid,
    carol: Uuid,
    group: Uuid,
}

async fn household(db: &impl Db) -> DbResult<Household> {
    let (alice, bob, carol) = (user(db, "alice").await?, user(db, "bob").await?, user(db, "carol").await?);
    let group = uuid(0x6007);
    share(db, group, &[alice, bob]).await?;
    Ok(Household { alice, bob, carol, group })
}

async fn account(db: &impl Db, owner: Uuid, account_id: &str) -> DbResult<()> {
    insert(db, "accounts", data! {"user_id": owner, "account_id": account_id, "item_id": "item-1", "institution_id": "ins-1", "name": account_id}, &[], false)
        .await?;
    Ok(())
}

async fn txn(db: &impl Db, owner: Uuid, id: &str, account: &str, amount: impl Into<Value>, date: &str) -> DbResult<()> {
    insert(db, "transactions", data! {"user_id": owner, "transaction_id": id, "account_id": account, "amount": amount, "date": date}, &[], false).await?;
    Ok(())
}

/// The row with this primary key, which must be there.
async fn stored(db: &impl Db, table: &str, id: impl Into<Value>) -> DbResult<Row> {
    db.query_one(Query::table(table).key([id.into()])).await
}

/// Moves a row's `updated` into the past, so a test can tell the rows a
/// later write stamps from the ones it left alone.
async fn backdate(db: &impl Db, table: &str, id: impl Into<Value>, when: Timestamp) -> DbResult<()> {
    db.update(Update::table(table).key([id.into()]).set("updated", when)).await?;
    Ok(())
}

fn texts_of(rows: &Rows, name: &str) -> Vec<String> {
    column(rows, name)
}

fn dec(text: &str) -> Decimal {
    text.parse().expect("a decimal number")
}

/// The code a statement that had to fail failed with.
fn code<T: std::fmt::Debug>(result: DbResult<T>) -> ErrorCode {
    result.expect_err("the statement is refused").code
}

/// The calendar day of a point in time, in UTC.
fn day_of(t: Timestamp) -> Date {
    Date(t.micros().div_euclid(86_400_000_000) as i32)
}

// R-S1, R-S2, R-S3, R-S5, R-S6, R-O3: every start creates the tables that are missing, one statement per table, and
// leaves the ones that are there alone.
#[tokio::test(flavor = "multi_thread")]
async fn tables_are_created_from_the_models_and_left_alone_when_present() -> DbResult<()> {
    let (_server, db) = fresh().await;
    db.ping().await?;
    schema(&db).await?;

    let described = db.describe().await?;
    let names: BTreeSet<&str> = described.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, models().iter().map(|m| m.name.as_str()).collect::<BTreeSet<_>>());
    assert_eq!(names.len(), 23);

    // Columns keep the model's order: `SELECT *` and the catalog diff of
    // the migration both read them in it.
    for model in models() {
        let table = described.iter().find(|t| t.name == model.name).expect("the table was created");
        assert_eq!(column_names(table), column_names(model), "{}", model.name);
        assert_eq!(table.primary_key, model.primary_key, "{}", model.name);
        for (found, wanted) in table.columns.iter().zip(&model.columns) {
            assert_eq!((&found.ty, found.nullable), (&wanted.ty, wanted.nullable), "{}.{}", model.name, wanted.name);
        }
    }

    let transactions = described.iter().find(|t| t.name == "transactions").unwrap();
    assert_eq!(transactions.columns[transactions.column_index("amount").unwrap()].ty, Type::decimal(15, 2));
    let users = described.iter().find(|t| t.name == "users").unwrap();
    let by_email = users.find_index("idx_users_lower_email").expect("the expression index");
    assert!(by_email.unique && by_email.filter.is_some());
    let rejected = described.iter().find(|t| t.name == "rejected_categories").unwrap();
    assert_eq!(rejected.primary_key, ["transaction_id", "category_id"]);
    let to_category = rejected.foreign_keys.iter().find(|f| f.columns == ["category_id"]).expect("the foreign key");
    assert_eq!((to_category.ref_table.as_str(), to_category.on_delete), ("categories", OnDelete::Cascade));

    // The second start: every table is there, and is left exactly as it
    // is, rows included, even where the model has moved on.
    let ann = user(&db, "ann").await?;
    schema(&db).await?;
    let mut newer = model("charts").clone();
    newer.columns.push(opt("colour", Type::Text));
    db.create_table_if_not_exists(newer).await?;
    assert!(db.get("users", [ann]).await?.is_some());
    let charts = db.describe_table("charts").await?.expect("charts exists");
    assert_eq!(column_names(&charts), column_names(model("charts")));

    assert_eq!(code(db.create_table(model("users").clone()).await), ErrorCode::AlreadyExists);
    assert!(db.describe_table("no_such_table").await?.is_none());
    Ok(())
}

/// The name the application's processes agree on, where Postgres would have
/// an advisory lock key.
const MIGRATION_LOCK: &str = "app.migration";

/// A NOT NULL column without a default gets one inferred from its type, so
/// the rows already there have a value.
fn with_inferred_default(column: ColumnDef) -> ColumnDef {
    if column.nullable || column.default.is_some() {
        return column;
    }
    match column.ty.clone() {
        Type::Bool => column.default(false),
        Type::Int => column.default(0),
        Type::Uuid => column.default_random_uuid(),
        Type::Json => column.default(json!({})),
        Type::Timestamp => column.default_now(),
        _ => column.default(""),
    }
}

/// The migration's type rule: kinds are compared, precision is not.
fn same_kind(a: &Type, b: &Type) -> bool {
    match (a, b) {
        (Type::Decimal(_), Type::Decimal(_)) => true,
        (Type::List(a), Type::List(b)) => same_kind(a, b),
        _ => a == b,
    }
}

/// The body of the column migration: the catalog is diffed against the
/// models, every missing column is added, and a column of another kind is
/// fatal. A table that does not exist is skipped.
async fn add_missing_columns(tx: &Transaction, wanted: &[TableDef]) -> DbResult<Vec<String>> {
    let found = tx.describe().await?;
    let mut added = Vec::new();
    let mut problems = Vec::new();
    for model in wanted {
        let Some(table) = found.iter().find(|t| t.name == model.name) else { continue };
        for column in &model.columns {
            match table.columns.iter().find(|c| c.name == column.name) {
                None => {
                    tx.add_column(&model.name, with_inferred_default(column.clone())).await?;
                    added.push(format!("{}.{}", model.name, column.name));
                }
                Some(existing) if !same_kind(&existing.ty, &column.ty) => {
                    problems.push(format!("{}.{} is {}, the model says {}", model.name, column.name, existing.ty.name(), column.ty.name()));
                }
                Some(_) => {}
            }
        }
    }
    if problems.is_empty() {
        Ok(added)
    } else {
        Err(DbError::invalid(problems.join("; ")))
    }
}

/// The column migration: the lock is taken before the transaction and
/// given up after it, whatever happened; the additions land together or
/// not at all.
async fn migrate(db: &Client, wanted: &[TableDef]) -> DbResult<Vec<String>> {
    let guard = db.lock(MIGRATION_LOCK).await?;
    let tx = db.begin().await?;
    let outcome = match add_missing_columns(&tx, wanted).await {
        Ok(added) => tx.commit().await.map(|()| added),
        Err(problem) => {
            let _ = tx.rollback().await;
            Err(problem)
        }
    };
    let _ = guard.unlock().await;
    outcome
}

// R-S1, R-S8, R-X5, R-S11: the migration at start: the tables are created, the missing columns added in one
// transaction under a named lock, and the indexes built after them.
#[tokio::test(flavor = "multi_thread")]
async fn missing_columns_are_added_in_one_transaction_under_the_migration_lock() -> DbResult<()> {
    let (_server, db) = fresh().await;

    // The database as the previous release left it: these columns, and
    // the index over one of them, came later.
    let newer = ["source", "auth_sid", "auth_refresh_token", "auth_recheck_at", "plan_fund_mappings"];
    for model in models() {
        let mut old = model.clone();
        old.columns.retain(|c| !newer.contains(&c.name.as_str()));
        old.indexes.retain(|i| i.name != "idx_sessions_auth_sid");
        db.create_table(old).await?;
    }
    let ann = user(&db, "ann").await?;
    account(&db, ann, "acc-1").await?;
    txn(&db, ann, "t-old", "acc-1", 12.5, "2026-03-05").await?;
    insert(&db, "sessions", data! {"session_id": "s-old", "user_user_id": ann}, &[], false).await?;

    // Creating the tables again changes none of them.
    schema(&db).await?;
    let before = db.describe_table("transactions").await?.expect("transactions exists");
    assert!(before.column_index("source").is_none());

    // Processes starting together take turns: one holder at a time.
    let held = db.lock(MIGRATION_LOCK).await?;
    assert!(db.try_lock(MIGRATION_LOCK).await?.is_none());
    held.unlock().await?;

    let mut added = migrate(&db, models()).await?;
    added.sort();
    assert_eq!(
        added,
        [
            "accounts.plan_fund_mappings",
            "investment_transactions.source",
            "sessions.auth_recheck_at",
            "sessions.auth_refresh_token",
            "sessions.auth_sid",
            "transactions.source"
        ]
    );
    let free = db.try_lock(MIGRATION_LOCK).await?.expect("the migration gave its lock up");
    free.unlock().await?;

    // Rows written before the column existed read its default; a nullable
    // column without one reads NULL.
    assert_eq!(stored(&db, "transactions", "t-old").await?.get::<String>("source")?, "bank");
    assert_eq!(stored(&db, "sessions", "s-old").await?.get::<Option<String>>("auth_sid")?, None);
    assert!(stored(&db, "accounts", "acc-1").await?.is_null("plan_fund_mappings")?);
    txn(&db, ann, "t-new", "acc-1", 1, "2026-03-06").await?;
    update(&db, "transactions", "t-new", data! {"source": "manual"}, Scope::Owner(ann), vec![]).await?;
    assert_eq!(stored(&db, "transactions", "t-new").await?.get::<String>("source")?, "manual");

    // Indexes come after the columns, so one can cover a column just added.
    for model in models() {
        for index in &model.indexes {
            db.create_index_if_not_exists(&model.name, index.clone()).await?;
        }
    }
    update(&db, "sessions", "s-old", data! {"auth_sid": "sid-1"}, Scope::Everyone, vec![]).await?;
    let by_sid = db.query(Query::table("sessions").index("idx_sessions_auth_sid", KeyRange::eq(["sid-1"]))).await?;
    assert_eq!(texts_of(&by_sid, "session_id"), ["s-old"]);

    // A second run finds nothing to do.
    assert!(migrate(&db, models()).await?.is_empty());

    // A column of the wrong kind refuses the start, and the column added
    // earlier in the same transaction goes with the rollback.
    let mut wrong = vec![model("charts").clone(), model("budgets").clone()];
    wrong[0].columns.push(opt("pinned", Type::Bool));
    wrong[1].columns.iter_mut().find(|c| c.name == "name").unwrap().ty = Type::Int;
    let refused = migrate(&db, &wrong).await.unwrap_err();
    assert!(refused.message.contains("budgets.name"), "{refused}");
    let charts = db.describe_table("charts").await?.expect("charts exists");
    assert!(charts.column_index("pinned").is_none());
    let free = db.try_lock(MIGRATION_LOCK).await?.expect("a failed migration gives its lock up too");
    drop(free);

    // A guard that is dropped, as when its process dies, frees the lock:
    // the next taker waits for the server to notice, and gets it.
    db.lock(MIGRATION_LOCK).await?.unlock().await?;
    Ok(())
}

// R-S11, R-S3, R-Q3, R-Q11, R-W11: a unique expression index comes to a table that has rows: the check for
// duplicates before it, and the index once they are gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_unique_index_is_built_only_once_its_duplicates_are_gone() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    // An older database: users, but not yet the index over their emails.
    db.drop_index("users", "idx_users_lower_email").await?;
    for (name, email) in [("ann", Some("Ann@Example.com")), ("ann2", Some("ann@example.COM")), ("bob", Some("bob@example.com")), ("cat", None), ("dan", None)] {
        insert(&db, "users", data! {"username": name, "email": email}, &[], false).await?;
    }

    // The keys held more than once, among the rows the index would cover.
    // Users without an email are not duplicates of each other.
    let duplicates = || {
        Query::table("users")
            .filter(col("email").is_not_null())
            .group_by([col("email").lower()])
            .having(count().gt(1))
            .select("email", col("email").lower())
            .select("n", count())
            .order_by(col("email").lower())
    };
    let found = db.query(duplicates()).await?;
    assert_eq!(texts_of(&found, "email"), ["ann@example.com"]);
    assert_eq!(column::<i64>(&found, "n"), [2]);

    // The application refuses to start instead of trying. Were it to try,
    // the index is refused and nothing of it is left behind.
    let refused = db.create_index("users", lower_email_index()).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::UniqueViolation);
    assert!(refused.violates("idx_users_lower_email"));
    let users = db.describe_table("users").await?.expect("users exists");
    assert!(users.find_index("idx_users_lower_email").is_none());

    update(&db, "users", stored_user(&db, "ann2").await?, data! {"email": "second@example.com"}, Scope::Everyone, vec![]).await?;
    assert!(db.query(duplicates()).await?.is_empty());
    db.create_index("users", lower_email_index()).await?;
    db.create_index_if_not_exists("users", lower_email_index()).await?;
    assert_eq!(code(db.create_index("users", lower_email_index()).await), ErrorCode::AlreadyExists);

    // From now on the index refuses what the check looked for, and serves
    // the case-insensitive lookup.
    let taken = insert(&db, "users", data! {"username": "eve", "email": "BOB@example.com"}, &[], false).await.unwrap_err();
    assert_eq!(taken.code, ErrorCode::UniqueViolation);
    assert!(taken.violates("idx_users_lower_email"));
    let bob = db.query_one(Query::table("users").index("idx_users_lower_email", KeyRange::eq(["bob@example.com"]))).await?;
    assert_eq!(bob.get::<String>("username")?, "bob");
    insert(&db, "users", data! {"username": "fay", "email": None::<String>}, &[], false).await?;
    Ok(())
}

/// A user's id by name, through the unique index.
async fn stored_user(db: &impl Db, username: &str) -> DbResult<Uuid> {
    db.query_one(Query::table("users").index("users_username_key", KeyRange::eq([username]))).await?.get("user_id")
}

/// The ids of one user's transactions that pass a condition, in id order.
async fn matching(db: &impl Db, owner: Uuid, condition: Expr) -> DbResult<Vec<String>> {
    db.query(Query::table("transactions").index("idx_transactions_user_id_transaction_id", KeyRange::eq([owner])).filter(condition).columns(["transaction_id"]))
        .await?
        .column()
}

// R-T5, R-T1, R-W1, R-W6: amounts bound as floats, integers and text, by an insert, an update and an upsert.
#[tokio::test(flavor = "multi_thread")]
async fn floats_written_to_money_columns_are_rounded_half_away_from_zero_to_the_cent() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;

    // What a caller binds, and what `NUMERIC(15,2)` then holds. A float
    // goes through its first 15 significant digits, so 2.675 (a hair under
    // in binary) and 0.1 + 0.2 (a hair over) land where they print.
    let cases: Vec<(Value, &str)> = vec![
        (12.345.into(), "12.35"),
        ((-12.345).into(), "-12.35"),
        (2.675.into(), "2.68"),
        (1.005.into(), "1.01"),
        ((0.1 + 0.2).into(), "0.30"),
        (0.004999.into(), "0.00"),
        ((-0.005).into(), "-0.01"),
        (1e-9.into(), "0.00"),
        (5.into(), "5.00"),
        ("19.999".into(), "20.00"),
        (dec("7.125").into(), "7.13"),
        (9999999999999.99.into(), "9999999999999.99"),
        ((-9999999999999.99).into(), "-9999999999999.99"),
    ];
    for (i, (written, kept)) in cases.into_iter().enumerate() {
        let id = format!("t-{i:02}");
        let returned = insert(
            &db,
            "transactions",
            data! {"user_id": ann, "transaction_id": id.as_str(), "account_id": "acc-1", "amount": written, "date": "2026-03-05"},
            &["amount"],
            false,
        )
        .await?
        .one()?;
        // The insert already answers with what was stored, not what was sent.
        assert_eq!(returned.get::<Decimal>("amount")?.to_string(), kept, "{id}");
        let row = stored(&db, "transactions", id.as_str()).await?;
        assert_eq!(row.get::<Decimal>("amount")?.to_string(), kept, "{id}");
        // Read as a float, it is the float parsed from that text.
        assert_eq!(row.get::<f64>("amount")?, kept.parse::<f64>().unwrap(), "{id}");
    }

    // An update and an upsert round the same way.
    update(&db, "transactions", "t-00", data! {"amount": 0.125}, Scope::Owner(ann), vec![]).await?;
    assert_eq!(stored(&db, "transactions", "t-00").await?.get::<Decimal>("amount")?, dec("0.13"));
    let upserted = upsert(
        &db,
        "transactions",
        data! {"user_id": ann, "transaction_id": "t-01", "account_id": "acc-1", "amount": 33.335, "date": "2026-03-05"},
        None,
        None,
    )
    .await?
    .one()?;
    assert_eq!(upserted.get::<Decimal>("amount")?.to_string(), "33.34");

    // NULL stays NULL, and reads as no number at all.
    txn(&db, ann, "t-null", "acc-1", Value::Null, "2026-03-05").await?;
    assert_eq!(stored(&db, "transactions", "t-null").await?.get::<Option<f64>>("amount")?, None);
    // The column default of a split's amount is an exact zero.
    let split =
        insert(&db, "split_transactions", data! {"user_id": ann, "transaction_id": "t-00", "account_id": "acc-1", "date": "2026-03-05"}, &["amount"], false)
            .await?
            .one()?;
    assert_eq!(split.get::<Decimal>("amount")?.to_string(), "0.00");
    Ok(())
}

// R-T5, R-W11, R-W2: numeric overflow (22003 in Postgres) on money and price columns.
#[tokio::test(flavor = "multi_thread")]
async fn amounts_too_large_for_their_column_are_out_of_range() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;

    // `NUMERIC(15,2)` holds up to 9999999999999.99. One cent more does not
    // fit, nor does a value that only reaches 14 whole digits by rounding.
    let too_large: Vec<Value> = vec![
        1e13.into(),
        (-1e13).into(),
        9999999999999.996.into(),
        "9999999999999.995".into(),
        "10000000000000".into(),
        10_000_000_000_000_i64.into(),
        dec("-9999999999999.999").into(),
    ];
    for (i, amount) in too_large.into_iter().enumerate() {
        let refused = txn(&db, ann, &format!("t-{i}"), "acc-1", amount, "2026-03-05").await;
        assert_eq!(code(refused), ErrorCode::OutOfRange, "case {i}");
    }
    assert!(matching(&db, ann, val(true)).await?.is_empty());

    // `NUMERIC(15,6)` holds up to 999999999.999999.
    upsert(&db, "securities", data! {"security_id": "sec-1", "close_price": "999999999.999999"}, None, None).await?;
    for price in [Value::from(1e9), Value::from("999999999.9999995"), Value::from(1_000_000_000)] {
        let refused = upsert(&db, "securities", data! {"security_id": "sec-1", "close_price": price}, None, None).await;
        assert_eq!(code(refused), ErrorCode::OutOfRange);
    }
    assert_eq!(stored(&db, "securities", "sec-1").await?.get::<Decimal>("close_price")?.to_string(), "999999999.999999");

    // One bad row refuses the whole statement: the rows beside it are not
    // stored either.
    let rows = Insert::into("transactions")
        .columns(["user_id", "transaction_id", "account_id", "amount", "date"])
        .values([val(ann), val("ok-1"), val("acc-1"), val(1.5), val("2026-03-05")])
        .values([val(ann), val("too-big"), val("acc-1"), val(1e13), val("2026-03-05")])
        .values([val(ann), val("ok-2"), val("acc-1"), val(2.5), val("2026-03-05")]);
    assert_eq!(code(db.insert(rows).await), ErrorCode::OutOfRange);
    assert!(matching(&db, ann, val(true)).await?.is_empty());

    // A result too large for the column is refused where it is stored,
    // and the row keeps what it had.
    txn(&db, ann, "t-big", "acc-1", 9999999999999.99, "2026-03-05").await?;
    let grown = db.update(Update::table("transactions").key(["t-big"]).set("amount", col("amount") * 10)).await;
    assert_eq!(code(grown), ErrorCode::OutOfRange);
    assert_eq!(stored(&db, "transactions", "t-big").await?.get::<Decimal>("amount")?, dec("9999999999999.99"));
    Ok(())
}

// R-T5: `NUMERIC(15,6)` for a security's price, a holding, a trade and an index value.
#[tokio::test(flavor = "multi_thread")]
async fn prices_and_quantities_keep_six_decimal_places() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;

    let security = upsert(
        &db,
        "securities",
        data! {"security_id": "sec-1", "ticker_symbol": "VTI", "close_price": 123.4567891, "close_price_as_of": "2026-03-05"},
        None,
        None,
    )
    .await?
    .one()?;
    assert_eq!(security.get::<Decimal>("close_price")?.to_string(), "123.456789");
    assert_eq!(security.get::<f64>("close_price")?, 123.456789);

    // Half a millionth rounds away from zero; less than that is gone.
    let holding = upsert(
        &db,
        "holdings",
        data! {
            "user_id": ann, "holding_id": "acc-1-sec-1", "account_id": "acc-1", "security_id": "sec-1", "institution_price": 0.0000005,
            "quantity": -0.0000005, "institution_value": 1234.565, "cost_basis": 0.0000004,
        },
        None,
        None,
    )
    .await?
    .one()?;
    assert_eq!(holding.get::<Decimal>("institution_price")?.to_string(), "0.000001");
    assert_eq!(holding.get::<Decimal>("quantity")?.to_string(), "-0.000001");
    // The money columns of the same row keep two places.
    assert_eq!(holding.get::<Decimal>("institution_value")?.to_string(), "1234.57");
    assert_eq!(holding.get::<Decimal>("cost_basis")?.to_string(), "0.00");

    let trade = insert(
        &db,
        "investment_transactions",
        data! {
            "user_id": ann, "investment_transaction_id": "inv-1", "account_id": "acc-1", "security_id": "sec-1", "date": "2026-03-05",
            "amount": 1543.21, "quantity": 12.5, "price": "123.4568",
        },
        &["*"],
        false,
    )
    .await?
    .one()?;
    assert_eq!(trade.get::<Decimal>("quantity")?.to_string(), "12.500000");
    assert_eq!(trade.get::<Decimal>("price")?.to_string(), "123.456800");
    assert_eq!(trade.get::<Decimal>("amount")?.to_string(), "1543.21");
    assert_eq!((trade.get::<f64>("quantity")?, trade.get::<f64>("price")?, trade.get::<f64>("amount")?), (12.5, 123.4568, 1543.21));

    let cpi = upsert_many(
        &db,
        "cpi_snapshots",
        vec![data! {"cpi_snapshot_id": "CUUR0000SA0-2026-01", "series_id": "CUUR0000SA0", "snapshot_date": "2026-01-01", "index_value": 325.2519999}],
        None,
    )
    .await?;
    assert_eq!(cpi[0].get::<Decimal>("index_value")?.to_string(), "325.252000");

    // A price compares exactly with the float it was written from.
    let found = db.query(Query::table("securities").filter(col("close_price").eq(123.456789))).await?;
    assert_eq!(texts_of(&found, "security_id"), ["sec-1"]);
    assert!(db.query(Query::table("securities").filter(col("close_price").eq(123.4567891))).await?.is_empty());
    Ok(())
}

// R-T5, R-Q11, R-W2: what is stored is exact decimal, and so is its `SUM`.
#[tokio::test(flavor = "multi_thread")]
async fn sums_of_money_are_exact_where_float_addition_drifts() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;

    // 3,000 dimes on one account, and every amount from one cent to ten
    // dollars on another, 500 rows to a statement.
    let mut rows: Vec<(String, &str, f64)> = (0..3000).map(|i| (format!("dime-{i:04}"), "acc-dimes", 0.1)).collect();
    rows.extend((1..=1000).map(|i| (format!("cent-{i:04}"), "acc-cents", f64::from(i) / 100.0)));
    for chunk in rows.chunks(500) {
        let mut insert = Insert::into("transactions").columns(["user_id", "transaction_id", "account_id", "amount", "date"]);
        for (id, account, amount) in chunk {
            insert = insert.values([val(ann), val(id), val(*account), val(*amount), val("2026-03-05")]);
        }
        assert_eq!(db.insert(insert).await?.affected(), 500);
    }

    // The same additions in floats miss both totals.
    assert_ne!((0..3000).fold(0.0, |total, _| total + 0.1), 300.0);
    assert_ne!((1..=1000).fold(0.0, |total, i| total + f64::from(i) / 100.0), 5005.0);

    let per_account = db
        .query(
            Query::table("transactions")
                .index("idx_transactions_user_id_transaction_id", KeyRange::eq([ann]))
                .group_by([col("account_id")])
                .select("account_id", col("account_id"))
                .select("total", sum(col("amount")))
                .select("n", count())
                .select("smallest", min(col("amount")))
                .select("largest", max(col("amount")))
                .order_by(col("account_id")),
        )
        .await?;
    assert_eq!(texts_of(&per_account, "account_id"), ["acc-cents", "acc-dimes"]);
    assert_eq!(column::<i64>(&per_account, "n"), [1000, 3000]);
    let totals: Vec<String> = column::<Decimal>(&per_account, "total").iter().map(Decimal::to_string).collect();
    assert_eq!(totals, ["5005.00", "300.00"]);
    assert_eq!(column::<f64>(&per_account, "total"), [5005.0, 300.0]);
    assert_eq!(column::<Decimal>(&per_account, "smallest"), [dec("0.01"), dec("0.1")]);
    assert_eq!(column::<Decimal>(&per_account, "largest"), [dec("10"), dec("0.1")]);

    // Money out and the same money back cancel to exactly zero.
    for chunk in rows.chunks(500) {
        let mut insert = Insert::into("transactions").columns(["user_id", "transaction_id", "account_id", "amount", "date"]);
        for (id, account, amount) in chunk {
            insert = insert.values([val(ann), val(format!("back-{id}")), val(*account), val(-*amount), val("2026-03-06")]);
        }
        db.insert(insert).await?;
    }
    let all = db
        .query_one(
            Query::table("transactions")
                .index("idx_transactions_user_id_transaction_id", KeyRange::eq([ann]))
                .select("total", sum(col("amount")))
                .select("n", count()),
        )
        .await?;
    assert_eq!(all.get::<i64>("n")?, 8000);
    assert!(all.get::<Decimal>("total")?.is_zero());
    assert_eq!(all.get::<Decimal>("total")?.to_string(), "0.00");
    Ok(())
}

// R-T5, R-Q16, R-Q9, R-T9: decimal amounts compared exactly with float and integer parameters
// (`t2.amount = -t1.amount`, `amount <> 0`, `BETWEEN` floats, `SIGN`, `ABS`, `label_category_confidence = 1`).
#[tokio::test(flavor = "multi_thread")]
async fn decimal_columns_compare_exactly_with_float_and_integer_parameters() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    txn(&db, ann, "a-out", "acc-1", 5.75, "2026-03-05").await?;
    txn(&db, ann, "b-in", "acc-2", "-5.75", "2026-03-06").await?;
    txn(&db, ann, "c-dime", "acc-1", 0.1, "2026-03-05").await?;
    txn(&db, ann, "d-third", "acc-1", 33.33, "2026-03-05").await?;
    txn(&db, ann, "e-zero", "acc-1", 0, "2026-03-05").await?;
    txn(&db, ann, "f-none", "acc-1", Value::Null, "2026-03-05").await?;
    let amount = || col("amount");

    // Whatever kind of number the parameter is, it is compared by value.
    assert_eq!(matching(&db, ann, amount().eq(0.1)).await?, ["c-dime"]);
    assert_eq!(matching(&db, ann, amount().eq(dec("0.10"))).await?, ["c-dime"]);
    assert_eq!(matching(&db, ann, amount().eq("0.1")).await?, ["c-dime"]);
    assert_eq!(matching(&db, ann, amount().eq(0)).await?, ["e-zero"]);
    assert!(matching(&db, ann, amount().eq(5)).await?.is_empty());
    assert_eq!(matching(&db, ann, amount().eq(33.33)).await?, ["d-third"]);
    assert!(matching(&db, ann, amount().gt(33.33)).await?.is_empty());
    assert_eq!(matching(&db, ann, amount().ge(33.33)).await?, ["d-third"]);
    // NULL is not "different from zero": the row is in neither answer.
    assert_eq!(matching(&db, ann, amount().ne(0)).await?, ["a-out", "b-in", "c-dime", "d-third"]);
    assert_eq!(matching(&db, ann, amount().between(5.0, 6.5)).await?, ["a-out"]);
    assert_eq!(matching(&db, ann, amount().between(-6, 0.1)).await?, ["b-in", "c-dime", "e-zero"]);

    // Negation, sign and magnitude are exact.
    assert_eq!(matching(&db, ann, (-amount()).eq(5.75)).await?, ["b-in"]);
    assert_eq!(matching(&db, ann, call(Func::Sign, [amount()]).eq(-1)).await?, ["b-in"]);
    assert_eq!(matching(&db, ann, call(Func::Sign, [amount()]).eq(0)).await?, ["e-zero"]);
    assert_eq!(matching(&db, ann, call(Func::Sign, [amount()]).eq(call(Func::Sign, [val(12.5)]))).await?, ["a-out", "c-dime", "d-third"]);
    assert_eq!(matching(&db, ann, call(Func::Abs, [amount()]).gt(5)).await?, ["a-out", "b-in", "d-third"]);
    assert_eq!(matching(&db, ann, call(Func::Abs, [amount()]).eq(dec("5.75"))).await?, ["a-out", "b-in"]);

    let computed = db
        .query(
            Query::table("transactions")
                .index("idx_transactions_user_id_transaction_id", KeyRange::eq([ann]))
                .select("transaction_id", col("transaction_id"))
                .select("sign", call(Func::Sign, [amount()]))
                .select("magnitude", call(Func::Abs, [amount()]))
                .select("negated", -amount())
                .select("signed_split", val(dec("12.34")) * call(Func::Sign, [amount()])),
        )
        .await?;
    assert_eq!(column::<Option<f64>>(&computed, "sign"), [Some(1.0), Some(-1.0), Some(1.0), Some(1.0), Some(0.0), None]);
    assert_eq!(
        column::<Option<Decimal>>(&computed, "magnitude"),
        [Some(dec("5.75")), Some(dec("5.75")), Some(dec("0.1")), Some(dec("33.33")), Some(dec("0")), None]
    );
    assert_eq!(
        column::<Option<Decimal>>(&computed, "negated"),
        [Some(dec("-5.75")), Some(dec("5.75")), Some(dec("-0.1")), Some(dec("-33.33")), Some(dec("0")), None]
    );
    assert_eq!(column::<Option<f64>>(&computed, "signed_split"), [Some(12.34), Some(-12.34), Some(12.34), Some(12.34), Some(0.0), None]);

    // The transfer match: the other row holds exactly the negated amount.
    // The negated value is also a key, read through the amount index.
    let mirrored = db
        .query(
            Query::table("transactions")
                .alias("t1")
                .index("idx_transactions_user_id_transaction_id", KeyRange::eq([ann]))
                .join(
                    Query::table("transactions")
                        .alias("t2")
                        .index("idx_transactions_user_id_amount_date", KeyRange::eq([val(ann), -col("t1.amount")]))
                        .filter(col("t1.amount").ne(0)),
                )
                .select("out", col("t1.transaction_id"))
                .select("back", col("t2.transaction_id")),
        )
        .await?;
    assert_eq!(
        mirrored.into_iter().map(|row| Ok((row.get("out")?, row.get("back")?))).collect::<DbResult<Vec<(String, String)>>>()?,
        [("a-out".to_string(), "b-in".to_string()), ("b-in".to_string(), "a-out".to_string())]
    );

    // The confidence marker is a float; a stored 1 is exactly 1.
    update(&db, "transactions", "a-out", data! {"label_category_confidence": 1}, Scope::Owner(ann), vec![]).await?;
    update(&db, "transactions", "b-in", data! {"label_category_confidence": 0.7}, Scope::Owner(ann), vec![]).await?;
    update(&db, "transactions", "c-dime", data! {"label_category_confidence": 0}, Scope::Owner(ann), vec![]).await?;
    let confidence = || col("label_category_confidence");
    assert_eq!(matching(&db, ann, confidence().eq(1)).await?, ["a-out"]);
    assert_eq!(matching(&db, ann, confidence().eq(1.0)).await?, ["a-out"]);
    assert_eq!(matching(&db, ann, confidence().ge(1)).await?, ["a-out"]);
    assert_eq!(matching(&db, ann, confidence().gt(0).and(confidence().lt(1))).await?, ["b-in"]);
    assert_eq!(matching(&db, ann, confidence().le(0)).await?, ["c-dime"]);
    assert_eq!(matching(&db, ann, confidence().is_null()).await?, ["d-third", "e-zero", "f-none"]);
    Ok(())
}

// R-T2, R-T3, R-T4: date, timestamp and uuid parameters travel as text, and come back typed.
#[tokio::test(flavor = "multi_thread")]
async fn dates_timestamps_and_uuids_written_as_text_are_parsed() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;

    // A uuid is read in either letter case and printed in lower case.
    let shouted = "6F9619FF-8B86-D011-B42D-00C04FC964FF";
    insert(&db, "users", data! {"user_id": shouted, "username": "ann"}, &[], false).await?;
    let ann: Uuid = stored(&db, "users", shouted.to_lowercase()).await?.get("user_id")?;
    assert_eq!(ann.to_string(), "6f9619ff-8b86-d011-b42d-00c04fc964ff");
    assert_eq!(ann, Uuid::parse(shouted).unwrap());
    assert!(db.get("users", [shouted]).await?.is_some());
    assert!(db.get("users", [ann]).await?.is_some());
    let by_text = db.query(Query::table("users").filter(col("user_id").eq(shouted))).await?;
    assert_eq!(by_text.len(), 1);

    // A calendar day goes in as `YYYY-MM-DD` and comes back as one.
    txn(&db, ann, "t-text", "acc-1", 1, "2026-03-05").await?;
    txn(&db, ann, "t-leap", "acc-1", 1, "2024-02-29").await?;
    insert(
        &db,
        "transactions",
        data! {"user_id": ann, "transaction_id": "t-typed", "account_id": "acc-1", "date": Date::from_ymd(2026, 3, 5).unwrap()},
        &[],
        false,
    )
    .await?;
    let day: Date = stored(&db, "transactions", "t-text").await?.get("date")?;
    assert_eq!(day, Date::from_ymd(2026, 3, 5).unwrap());
    assert_eq!(day.to_string(), "2026-03-05");
    assert_eq!(stored(&db, "transactions", "t-leap").await?.get::<Date>("date")?.ymd(), (2024, 2, 29));
    assert_eq!(matching(&db, ann, col("date").eq("2026-03-05")).await?, ["t-text", "t-typed"]);
    assert_eq!(matching(&db, ann, col("date").eq(day)).await?, ["t-text", "t-typed"]);
    assert_eq!(matching(&db, ann, col("date").lt("2026-01-01")).await?, ["t-leap"]);

    // A timestamp goes in as ISO 8601 with a zone, keeps its microseconds,
    // and is the same instant however the zone was written.
    let stamps = [
        ("s-utc", "2026-03-05T10:20:30.123456Z"),
        ("s-offset", "2026-03-05T12:20:30.123456+02:00"),
        ("s-space", "2026-03-05 10:20:30.123456+00"),
        ("s-millis", "2026-03-05T10:20:30.123Z"),
        ("s-day", "2026-03-05"),
    ];
    for (id, expires) in stamps {
        upsert(&db, "sessions", data! {"session_id": id, "cookie_expires": expires}, None, None).await?;
    }
    let instant = ts("2026-03-05T10:20:30.123456Z");
    for id in ["s-utc", "s-offset", "s-space"] {
        assert_eq!(stored(&db, "sessions", id).await?.get::<Timestamp>("cookie_expires")?, instant, "{id}");
    }
    // A caller may truncate to milliseconds on the way out; the stored
    // value is the microsecond one.
    assert_eq!(instant.micros() % 1000, 456);
    assert_eq!(Timestamp::from_millis(instant.millis()), stored(&db, "sessions", "s-millis").await?.get::<Timestamp>("cookie_expires")?);
    assert_eq!(instant.to_rfc3339(), "2026-03-05T10:20:30.123456Z");
    // A bare date in a timestamp column is that day's midnight, UTC.
    assert_eq!(stored(&db, "sessions", "s-day").await?.get::<Timestamp>("cookie_expires")?, ts("2026-03-05T00:00:00Z"));

    // Text compared with a timestamp column is read as a timestamp too.
    let later = db.query(Query::table("sessions").filter(col("cookie_expires").gt("2026-03-05T10:20:30.123Z"))).await?;
    assert_eq!(texts_of(&later, "session_id"), ["s-offset", "s-space", "s-utc"]);
    let exactly = db.query(Query::table("sessions").filter(col("cookie_expires").eq("2026-03-05T11:20:30.123456+01:00"))).await?;
    assert_eq!(exactly.len(), 3);

    // The server's own clock fills `updated` and `created_at`, and reads
    // as a timestamp like any other.
    let before = db.now().await?;
    upsert(&db, "sessions", data! {"session_id": "s-now"}, None, None).await?;
    let row = stored(&db, "sessions", "s-now").await?;
    assert!(row.get::<Timestamp>("updated")? >= before && row.get::<Timestamp>("created_at")? >= before);
    assert!(row.get::<Timestamp>("updated")? <= db.now().await?);
    Ok(())
}

// R-Q22, R-T2, R-T4, R-W11: a value that cannot be read as its column's type is an error (22P02, 22007, 42883 in
// Postgres): a cursor that is no timestamp, an id that is no uuid, a number for a text id.
#[tokio::test(flavor = "multi_thread")]
async fn unreadable_dates_and_ids_are_type_errors_not_empty_results() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    account(&db, ann, "acc-1").await?;
    txn(&db, ann, "t-1", "acc-1", 1, "2026-03-05").await?;
    upsert(&db, "items", data! {"item_id": "item-1", "user_id": ann}, None, None).await?;
    upsert(
        &db,
        "snapshots",
        data! {"snapshot_id": "acc-1-20260305", "user_id": ann, "snapshot_date": "2026-03-05T00:00:00Z", "snapshot_type": "holding"},
        None,
        None,
    )
    .await?;
    db.insert(
        Insert::into("recurrings")
            .set("recurring_id", uuid(1))
            .set("user_id", ann)
            .set("merchant_key", "netflix")
            .set("merchant_name", "Netflix")
            .set("cadence", "monthly")
            .set("is_income", false)
            .set("status", "suggested"),
    )
    .await?;

    // A delta is asked for since `yesterday`: the cursor cannot be read as
    // a timestamp, whether it is compared or used as the bound of a range.
    let compared = find(Query::table("recurrings"), &Filters::new(Scope::Group(ann)).since("updated", "yesterday"));
    assert_eq!(code(db.query(compared).await), ErrorCode::TypeMismatch);
    let as_bound =
        Query::table("recurrings").index("idx_recurrings_user_id_updated", KeyRange::within([ann], Bound::included(["yesterday"]), Bound::Unbounded));
    assert_eq!(code(db.query(as_bound).await), ErrorCode::TypeMismatch);
    // A day that does not exist is no more readable.
    let no_such_day = find(Query::table("transactions"), &Filters::new(Scope::Owner(ann)).since("date", "2026-02-30"));
    assert_eq!(code(db.query(no_such_day).await), ErrorCode::TypeMismatch);
    let no_such_instant = find(Query::table("snapshots"), &Filters::new(Scope::Owner(ann)).until("snapshot_date", "2026-02-30T00:00:00Z"));
    assert_eq!(code(db.query(no_such_instant).await), ErrorCode::TypeMismatch);

    // A read and a write of a recurring by an id that is no uuid.
    assert_eq!(code(db.get("recurrings", ["not-a-uuid"]).await), ErrorCode::TypeMismatch);
    assert_eq!(code(update(&db, "recurrings", "not-a-uuid", data! {"status": "dismissed"}, Scope::Owner(ann), vec![]).await), ErrorCode::TypeMismatch);
    // One bad id fails the whole batch, which is why callers validate.
    let one_bad_id =
        find(Query::table("recurrings"), &Filters::new(Scope::Owner(ann)).any_of("recurring_id", [Value::from("not-a-uuid"), Value::from(uuid(1))]));
    assert_eq!(code(db.query(one_bad_id).await), ErrorCode::TypeMismatch);
    assert_eq!(code(db.query(Query::table("recurrings").filter(col("user_id").eq("someone"))).await), ErrorCode::TypeMismatch);

    // A webhook that carries a number where the id is text.
    assert_eq!(code(db.query(Query::table("items").filter(col("item_id").eq(5))).await), ErrorCode::TypeMismatch);
    assert_eq!(code(db.get("items", [5]).await), ErrorCode::TypeMismatch);
    assert_eq!(
        code(find_one(&db, Query::table("snapshots").key([20260305]), &Filters::new(Scope::Group(ann)).eq("snapshot_type", "holding")).await),
        ErrorCode::TypeMismatch
    );

    // The same holds for what is written: text is parsed for the column,
    // and anything it cannot be is refused.
    assert_eq!(code(txn(&db, ann, "t-bad-1", "acc-1", 1, "tomorrow").await), ErrorCode::TypeMismatch);
    assert_eq!(code(txn(&db, ann, "t-bad-2", "acc-1", 1, "2026-02-29").await), ErrorCode::TypeMismatch);
    assert_eq!(code(txn(&db, ann, "t-bad-3", "acc-1", "12,50", "2026-03-05").await), ErrorCode::TypeMismatch);
    assert_eq!(code(txn(&db, ann, "t-bad-4", "acc-1", true, "2026-03-05").await), ErrorCode::TypeMismatch);
    assert_eq!(code(insert(&db, "users", data! {"user_id": "not-a-uuid", "username": "bob"}, &[], false).await), ErrorCode::TypeMismatch);
    assert_eq!(code(insert(&db, "users", data! {"username": 7}, &[], false).await), ErrorCode::TypeMismatch);
    assert_eq!(code(upsert(&db, "sessions", data! {"session_id": "s-1", "cookie_expires": "soon"}, None, None).await), ErrorCode::TypeMismatch);
    // Postgres reads decimal text into a BIGINT; here the caller sends the
    // integer it has.
    assert_eq!(code(upsert(&db, "sessions", data! {"session_id": "s-1", "cookie_max_age": "604800000"}, None, None).await), ErrorCode::TypeMismatch);
    upsert(&db, "sessions", data! {"session_id": "s-1", "cookie_max_age": 604_800_000_i64}, None, None).await?;
    assert_eq!(stored(&db, "sessions", "s-1").await?.get::<f64>("cookie_max_age")?, 604_800_000.0);
    assert_eq!(code(update(&db, "transactions", "t-1", data! {"label_budget_id": "none"}, Scope::Owner(ann), vec![]).await), ErrorCode::TypeMismatch);

    // None of it left anything behind, and a readable value still gets
    // its answer, empty or not.
    assert_eq!(matching(&db, ann, val(true)).await?, ["t-1"]);
    assert!(stored_user(&db, "bob").await.is_err());
    let far_future = find(Query::table("recurrings"), &Filters::new(Scope::Group(ann)).since("updated", "2099-01-01"));
    assert!(db.query(far_future).await?.is_empty());
    assert!(db.get("recurrings", ["6f9619ff-8b86-d011-b42d-00c04fc964ff"]).await?.is_none());
    Ok(())
}

// R-T6, R-Q13, R-Q7: JSON is stored whole and returned in jsonb key order, which a reader sees through
// `plan_fund_mappings` and `charts.configuration`; a path into it is read inside a statement
// (`raw->'category_hint'->>'primary'`).
#[tokio::test(flavor = "multi_thread")]
async fn json_is_stored_whole_and_returned_in_jsonb_key_order() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    account(&db, ann, "acc-401k").await?;

    // A reader lists the funds in the order the keys come back: shorter
    // keys first, then by bytes, at every depth.
    let mappings = json!({
        "Vanguard Target 2050": {"security_id": "sec-vt", "restated": false},
        "Zeta": {"security_id": "sec-z", "restated": true},
        "Bond": {"security_id": "sec-b", "restated": false},
        "International Fund": {"security_id": "sec-i", "restated": false},
        "Alpha": {"security_id": "sec-a", "restated": true},
    });
    let outcome = update(&db, "accounts", "acc-401k", data! {"plan_fund_mappings": mappings.clone()}, Scope::Owner(ann), vec![]).await?;
    assert_eq!(update_status(&outcome), 200);
    let kept: Json = stored(&db, "accounts", "acc-401k").await?.get("plan_fund_mappings")?;
    assert_eq!(
        kept.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
        ["Bond", "Zeta", "Alpha", "International Fund", "Vanguard Target 2050"]
    );
    assert_eq!(kept["Bond"].to_string(), r#"{"restated":false,"security_id":"sec-b"}"#);
    assert_eq!(kept, mappings);

    // A chart's configuration arrives as text: the application parses
    // it, the database stores the value, and the application serializes
    // what it reads back.
    let sent = r#"{ "title": "Net worth", "series": [ {"name": "b", "id": 2}, {"id": 1, "name": "a"} ], "type": "line", "y": {"min": 0, "max": 10.5} }"#;
    assert_eq!(code(insert(&db, "charts", data! {"user_id": ann, "configuration": sent}, &[], false).await), ErrorCode::TypeMismatch);
    let parsed: Json = serde_json::from_str(sent).unwrap();
    let chart = insert(&db, "charts", data! {"user_id": ann, "name": "Worth", "type": "line", "configuration": parsed}, &["*"], false).await?.one()?;
    assert_eq!(
        chart.get::<Json>("configuration")?.to_string(),
        r#"{"y":{"max":10.5,"min":0},"type":"line","title":"Net worth","series":[{"id":2,"name":"b"},{"id":1,"name":"a"}]}"#
    );

    // An array keeps its order; only object keys are sorted.
    let capacities = json!([{"month": "2026-02", "amount": 300}, {"amount": 250.5, "month": "2026-01"}]);
    let budget = insert(&db, "budgets", data! {"user_id": ann, "name": "Home", "capacities": capacities}, &["*"], false).await?.one()?;
    assert_eq!(budget.get::<Json>("capacities")?.to_string(), r#"[{"month":"2026-02","amount":300},{"month":"2026-01","amount":250.5}]"#);
    // Text inside survives as it is.
    let memo = json!({"note": "Dépenses \"café\" ☕ \\ 🙂", "tab\there": [null, true, 1e-7]});
    let noted = insert(&db, "charts", data! {"user_id": ann, "configuration": memo.clone()}, &["configuration"], false).await?.one()?;
    assert_eq!(noted.get::<Json>("configuration")?, memo);
    // A recurring's splits default to an empty array, not to NULL.
    db.insert(
        Insert::into("recurrings")
            .set("recurring_id", uuid(1))
            .set("user_id", ann)
            .set("merchant_key", "gym")
            .set("merchant_name", "Gym")
            .set("cadence", "monthly")
            .set("is_income", false)
            .set("status", "suggested"),
    )
    .await?;
    assert_eq!(stored(&db, "recurrings", uuid(1)).await?.get::<Json>("splits")?, json!([]));

    // The provider's payload is kept whole and read by path inside
    // statements. A missing step of the path gives NULL, never an error.
    let payloads = [
        ("t-out", Some(json!({"category_hint": {"primary": "TRANSFER_OUT", "detailed": "TRANSFER_OUT_SAVINGS"}, "amount": 5.5}))),
        ("t-food", Some(json!({"category_hint": {"primary": "FOOD_AND_DRINK"}}))),
        ("t-flat", Some(json!({"category_hint": "none"}))),
        ("t-empty", Some(json!({}))),
        ("t-none", None),
    ];
    for (id, raw) in payloads {
        upsert(
            &db,
            "transactions",
            data! {"user_id": ann, "transaction_id": id, "account_id": "acc-401k", "amount": 1, "date": "2026-03-05", "raw": raw},
            None,
            None,
        )
        .await?;
    }
    let primary = || col("raw").json_text(["category_hint", "primary"]);
    assert_eq!(matching(&db, ann, primary().eq("TRANSFER_OUT")).await?, ["t-out"]);
    assert_eq!(matching(&db, ann, primary().like("TRANSFER%")).await?, ["t-out"]);
    assert_eq!(matching(&db, ann, primary().not_like("TRANSFER%")).await?, ["t-food"]);
    assert_eq!(matching(&db, ann, primary().is_null()).await?, ["t-empty", "t-flat", "t-none"]);
    let read = db
        .query(
            Query::table("transactions")
                .key(["t-out"])
                .select("primary", primary())
                .select("category", col("raw").json(["category_hint"]))
                .select("amount", col("raw").json_text(["amount"]))
                .select("missing", col("raw").json(["location", "city"])),
        )
        .await?
        .one()?;
    assert_eq!(read.get::<String>("primary")?, "TRANSFER_OUT");
    assert_eq!(read.get::<Json>("category")?.to_string(), r#"{"primary":"TRANSFER_OUT","detailed":"TRANSFER_OUT_SAVINGS"}"#);
    assert_eq!(read.get::<String>("amount")?, "5.5");
    assert!(read.is_null("missing")?);

    // A builder update never writes `raw`; an upsert replaces it whole.
    update(&db, "transactions", "t-out", data! {"name": "Savings", "raw": json!({"overwritten": true})}, Scope::Owner(ann), vec![]).await?;
    assert_eq!(stored(&db, "transactions", "t-out").await?.get::<Json>("raw")?["amount"], json!(5.5));
    upsert(
        &db,
        "transactions",
        data! {"user_id": ann, "transaction_id": "t-out", "account_id": "acc-401k", "date": "2026-03-05", "raw": json!({"b": 1, "a": 2})},
        None,
        None,
    )
    .await?;
    let replaced = stored(&db, "transactions", "t-out").await?;
    assert_eq!(replaced.get::<Json>("raw")?.to_string(), r#"{"a":2,"b":1}"#);
    assert_eq!(replaced.get::<String>("name")?, "Savings");
    Ok(())
}

// R-Q2, R-Q1, R-Q6, R-Q3: the builder's reads (many rows, one row, rows by ids) with the scope, equality and
// `IS NULL` filters, `IN` lists and a case-insensitive match.
#[tokio::test(flavor = "multi_thread")]
async fn builder_reads_and_scope_filters_lists_and_the_live_predicate() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let rows = [
        ("acc-a1", home.alice, "item-a", "ins-1", "depository"),
        ("acc-a2", home.alice, "item-a", "ins-1", "credit"),
        ("acc-a3", home.alice, "item-b", "ins-2", "depository"),
        ("acc-b1", home.bob, "item-c", "ins-1", "depository"),
        ("acc-c1", home.carol, "item-d", "ins-1", "depository"),
    ];
    for (id, owner, item, institution, kind) in rows {
        insert(&db, "accounts", data! {"user_id": owner, "account_id": id, "item_id": item, "institution_id": institution, "type": kind}, &[], false).await?;
    }
    soft_delete(&db, "accounts", "acc-a2", Scope::Owner(home.alice)).await?;
    // A row whose flag is NULL, as rows older than the column are, is live.
    db.update(Update::table("accounts").key(["acc-a3"]).set("is_deleted", Value::Null)).await?;
    let ids = |rows: Rows| texts_of(&rows, "account_id");
    let group = Filters::new(Scope::Group(home.alice));

    // The whole group's live rows, whoever of it asks.
    assert_eq!(ids(db.query(find(Query::table("accounts"), &group)).await?), ["acc-a1", "acc-a3", "acc-b1"]);
    assert_eq!(ids(db.query(find(Query::table("accounts"), &Filters::new(Scope::Group(home.bob)))).await?), ["acc-a1", "acc-a3", "acc-b1"]);
    assert_eq!(ids(db.query(find(Query::table("accounts"), &Filters::new(Scope::Group(home.carol)))).await?), ["acc-c1"]);
    assert_eq!(ids(db.query(find(Query::table("accounts"), &Filters::new(Scope::Owner(home.alice)))).await?), ["acc-a1", "acc-a3"]);
    // With no owner at all: the hourly job reads everyone's.
    assert_eq!(db.query(find(Query::table("accounts"), &Filters::new(Scope::Everyone))).await?.len(), 4);
    // All rows come whole, in the model's column order.
    let whole = db.query(find(Query::table("accounts"), &group)).await?;
    assert_eq!(whole.columns(), all_columns("accounts"));

    // Equality filters are ANDed. The caller picks the way in; the filters
    // decide what of it is kept.
    let by_item = Query::table("accounts").index("idx_accounts_item_id", KeyRange::eq(["item-a"]));
    assert_eq!(ids(db.query(find(by_item.clone(), &group)).await?), ["acc-a1"]);
    assert_eq!(db.query(find(by_item.clone(), &group.clone().with_tombstones())).await?.len(), 2);
    assert_eq!(
        ids(db.query(find(Query::table("accounts"), &group.clone().eq("institution_id", "ins-1").eq("type", "depository"))).await?),
        ["acc-a1", "acc-b1"]
    );
    assert!(db.query(find(by_item, &Filters::new(Scope::Group(home.carol)))).await?.is_empty());
    // A null filter value means `IS NULL`.
    assert_eq!(
        ids(db.query(find(Query::table("accounts"), &group.clone().eq("custom_name", Value::Null).eq("type", "depository"))).await?),
        ["acc-a1", "acc-a3", "acc-b1"]
    );

    // An `IN` list; an empty one adds no condition.
    assert_eq!(ids(db.query(find(Query::table("accounts"), &group.clone().any_of("item_id", ["item-b", "item-c", "item-d"]))).await?), ["acc-a3", "acc-b1"]);
    assert_eq!(ids(db.query(find(Query::table("accounts"), &group.clone().any_of("item_id", Vec::<String>::new()))).await?), ["acc-a1", "acc-a3", "acc-b1"]);
    assert_eq!(
        ids(db.query(find(Query::table("accounts"), &group.clone().with_tombstones().any_of("account_id", ["acc-a2", "acc-c1", "acc-zz"]))).await?),
        ["acc-a2"]
    );

    // The first row or none.
    let one = find_one(&db, Query::table("accounts").key(["acc-b1"]), &group).await?.expect("a group member's account");
    assert_eq!(one.get::<Uuid>("user_id")?, home.bob);
    assert!(find_one(&db, Query::table("accounts").key(["acc-c1"]), &group).await?.is_none());
    assert!(find_one(&db, Query::table("accounts").key(["acc-a2"]), &group).await?.is_none());
    assert!(find_one(&db, Query::table("accounts"), &group.clone().eq("type", "loan")).await?.is_none());
    assert_eq!(find_one(&db, Query::table("accounts"), &group).await?.unwrap().get::<String>("account_id")?, "acc-a1");

    // By ids. Unknown, deleted and foreign ids are simply absent, and the
    // rows come in the order the ids were given.
    let asked: Vec<Value> = ["acc-b1", "acc-zz", "acc-a2", "acc-c1", "acc-a1"].map(Value::from).to_vec();
    let found = find_by_ids(&db, "accounts", asked, Scope::Group(home.alice)).await?;
    assert_eq!(found.iter().map(|row| row.get("account_id")).collect::<DbResult<Vec<String>>>()?, ["acc-b1", "acc-a1"]);
    assert!(find_by_ids(&db, "accounts", Vec::new(), Scope::Group(home.alice)).await?.is_empty());
    // A global table has neither an owner nor a flag.
    upsert_many(&db, "institutions", vec![data! {"institution_id": "ins-1", "name": "First"}, data! {"institution_id": "ins-2", "name": "Second"}], None)
        .await?;
    assert_eq!(find_by_ids(&db, "institutions", vec!["ins-2".into(), "ins-9".into()], Scope::Everyone).await?.len(), 1);

    // `lower(email) = lower($1)`, as a filter and through the index made
    // for it. Both fold the same way Rust's `to_lowercase` does.
    update(&db, "users", home.carol, data! {"email": "Carol.Émond@Example.com"}, Scope::Everyone, vec![]).await?;
    let by_filter = find_one(&db, Query::table("users"), &Filters::new(Scope::Everyone).ignoring_case("email", "CAROL.ÉMOND@example.COM")).await?;
    assert_eq!(by_filter.expect("found whatever the case").get::<Uuid>("user_id")?, home.carol);
    let key = "CAROL.ÉMOND@example.COM".to_lowercase();
    assert_eq!(key, "carol.émond@example.com");
    let by_index = find_one(&db, Query::table("users").index("idx_users_lower_email", KeyRange::eq([key])), &Filters::new(Scope::Everyone)).await?;
    assert_eq!(by_index.expect("found through the index").get::<Uuid>("user_id")?, home.carol);
    assert!(find_one(&db, Query::table("users").index("idx_users_lower_email", KeyRange::eq(["nobody@example.com"])), &Filters::new(Scope::Everyone))
        .await?
        .is_none());
    Ok(())
}

// R-Q4, R-Q21, R-W2, R-T1: the builder's stream, as a cold sync reads a table: keyset order on text keys, in
// 500-row pages with no shared snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn builder_batches_stream_a_table_in_key_order_500_rows_at_a_time() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;

    // 1,000 of Alice's rows, 250 of Bob's and 100 of Carol's, with ids of
    // the shapes providers and the application mint, so the key order is
    // not the order of insertion.
    let mut rows: Vec<(String, Uuid, &str)> = Vec::new();
    for i in 0..1250 {
        let id = match i % 5 {
            0 => format!("manual-{i:05}"),
            1 => format!("Bank{i:05}"),
            2 => format!("bank{i:05}"),
            3 => format!("{i:05}-x"),
            _ => format!("ZZ-{i:05}"),
        };
        rows.push((id, if i % 5 == 4 { home.bob } else { home.alice }, if i % 2 == 0 { "acc-even" } else { "acc-odd" }));
    }
    rows.extend((0..100).map(|i| (format!("Carol-{i:03}"), home.carol, "acc-carol")));
    for chunk in rows.chunks(500) {
        let mut insert = Insert::into("transactions").columns(["user_id", "transaction_id", "account_id", "amount", "date"]);
        for (id, owner, account) in chunk {
            insert = insert.values([val(*owner), val(id), val(*account), val(1), val("2026-03-05")]);
        }
        db.insert(insert).await?;
    }
    let ids_of = |pages: &[Vec<Row>]| pages.iter().flatten().map(|row| row.get("transaction_id")).collect::<DbResult<Vec<String>>>();
    let sizes = |pages: &[Vec<Row>]| pages.iter().map(Vec::len).collect::<Vec<_>>();
    // Text keys order by their bytes: digits, then capitals, then small
    // letters. `String`'s own order is the same.
    let mut group_ids: Vec<String> = rows.iter().filter(|(_, owner, _)| *owner != home.carol).map(|(id, _, _)| id.clone()).collect();
    group_ids.sort();
    assert!(group_ids[0].starts_with('0') && group_ids[1249].starts_with("manual"));

    // The whole group, tombstones included, as a cold sync asks.
    let group = Filters::new(Scope::Group(home.alice)).with_tombstones();
    let pages = find_in_batches(&db, "transactions", &group, BATCH).await?;
    assert_eq!(sizes(&pages), [500, 500, 250]);
    assert_eq!(ids_of(&pages)?, group_ids);

    // A total that is a multiple of the page size costs one more, empty,
    // statement to find the end.
    let pages = find_in_batches(&db, "transactions", &Filters::new(Scope::Owner(home.alice)), BATCH).await?;
    assert_eq!(sizes(&pages), [500, 500, 0]);
    // The optional filters narrow every page alike.
    let pages = find_in_batches(&db, "transactions", &group.clone().eq("account_id", "acc-odd").eq("pending", false), BATCH).await?;
    assert_eq!(sizes(&pages), [500, 125]);
    assert_eq!(ids_of(&pages)?.len(), rows.iter().filter(|(_, owner, account)| *owner != home.carol && *account == "acc-odd").count());
    let pages = find_in_batches(&db, "transactions", &Filters::new(Scope::Group(home.carol)), BATCH).await?;
    assert_eq!(sizes(&pages), [100]);
    assert!(find_in_batches(&db, "transactions", &group.clone().eq("account_id", "acc-none"), BATCH).await?.concat().is_empty());

    // Nothing ties the pages of a stream together. After the first page,
    // a row is added before the cursor and one after it, and a row still
    // to come is deleted.
    let snapshot = db.begin_read().await?;
    let first = snapshot.query(find(Query::table("transactions"), &group).limit(BATCH)).await?;
    let cursor: String = first.iter().last().expect("a full page").get("transaction_id")?;
    assert_eq!(cursor, group_ids[499]);
    txn(&db, home.alice, "00000-early", "acc-even", 1, "2026-03-05").await?;
    txn(&db, home.bob, "zzz-late", "acc-even", 1, "2026-03-05").await?;
    assert!(soft_delete(&db, "transactions", group_ids[900].as_str(), Scope::Group(home.alice)).await?);
    let rest = || find(Query::table("transactions").range(KeyRange::new(Bound::excluded([cursor.as_str()]), Bound::Unbounded)), &group);

    // On the pool, each page sees what is committed when it runs: the
    // early row is missed, the late one and the tombstone are seen.
    let on_pool = db.query(rest()).await?;
    let seen = texts_of(&on_pool, "transaction_id");
    assert_eq!(seen.len(), 751);
    assert!(seen.contains(&"zzz-late".to_string()) && !seen.contains(&"00000-early".to_string()));
    let deleted: Vec<String> =
        on_pool.iter().filter(|row| row.get::<Option<bool>>("is_deleted").unwrap() == Some(true)).map(|row| row.get("transaction_id").unwrap()).collect();
    assert_eq!(deleted, [group_ids[900].clone()]);

    // Inside a reading transaction the same pages are of one moment.
    let frozen = snapshot.query(rest()).await?;
    assert_eq!(texts_of(&frozen, "transaction_id"), group_ids[500..]);
    assert!(frozen.iter().all(|row| row.get::<Option<bool>>("is_deleted").unwrap() == Some(false)));
    snapshot.rollback().await?;
    Ok(())
}

// R-W1, R-S5, R-S6, R-S7, R-W11, R-Q8: the builder's insert: the values the server generates and the defaults
// inserts rely on; a constraint error is a failure of that row (23502, 23503, 23505, 22001 in Postgres).
#[tokio::test(flavor = "multi_thread")]
async fn builder_insert_takes_defaults_and_returns_the_stored_row() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let before = db.now().await?;

    // The whole row comes back, defaults filled in.
    let created = insert(
        &db,
        "accounts",
        data! {
            "user_id": ann, "account_id": "manual-1", "item_id": "manual", "institution_id": "manual", "name": "Cash", "type": "depository",
            "balances_current": 120.5, "balances_iso_currency_code": "USD",
        },
        &["*"],
        false,
    )
    .await?;
    assert_eq!((created.affected(), created.len(), status(&created)), (1, 1, 200));
    assert_eq!(created.columns(), all_columns("accounts"));
    let row = created.one()?;
    assert_eq!(
        (row.get::<Option<bool>>("hide")?, row.get::<Option<bool>>("archived")?, row.get::<Option<bool>>("is_deleted")?),
        (Some(false), Some(false), Some(false))
    );
    assert_eq!(
        (
            row.get::<bool>("graph_options_use_snapshots")?,
            row.get::<bool>("graph_options_use_holding_snapshots")?,
            row.get::<bool>("graph_options_use_transactions")?
        ),
        (true, true, true)
    );
    assert_eq!((row.get::<Option<String>>("custom_name")?, row.get::<Option<Decimal>>("balances_available")?), (None, None));
    assert_eq!(row.get::<Decimal>("balances_current")?, dec("120.50"));
    assert!(row.get::<Timestamp>("updated")? >= before);
    assert_eq!(row, stored(&db, "accounts", "manual-1").await?);

    // `source` says who made the row; left out, it is the provider's.
    let manual = insert(
        &db,
        "transactions",
        data! {
            "user_id": ann, "transaction_id": "manual-t1", "account_id": "manual-1", "name": "Rent", "amount": 900, "date": "2026-03-01",
            "pending": false, "source": "manual",
        },
        &["*"],
        false,
    )
    .await?
    .one()?;
    assert_eq!((manual.get::<String>("source")?, manual.get::<Option<bool>>("pending")?), ("manual".to_string(), Some(false)));
    txn(&db, ann, "bank-t1", "manual-1", 5, "2026-03-01").await?;
    let synced = stored(&db, "transactions", "bank-t1").await?;
    assert_eq!(
        (synced.get::<String>("source")?, synced.get::<Option<bool>>("pending")?, synced.get::<Option<f64>>("label_category_confidence")?),
        ("bank".to_string(), Some(false), None)
    );

    // The id is the server's, one per row, and the caller learns it from
    // what the insert returns.
    let budget = insert(&db, "budgets", data! {"user_id": ann}, &["*"], false).await?.one()?;
    let other = insert(
        &db,
        "budgets",
        data! {"user_id": ann, "name": "Trips", "iso_currency_code": "EUR", "roll_over": true, "roll_over_start_date": "2026-01-01"},
        &["*"],
        false,
    )
    .await?
    .one()?;
    assert_ne!(budget.get::<Uuid>("budget_id")?, other.get::<Uuid>("budget_id")?);
    assert_eq!(
        (budget.get::<String>("name")?, budget.get::<String>("iso_currency_code")?, budget.get::<bool>("roll_over")?),
        ("Unnamed".to_string(), "USD".to_string(), false)
    );
    assert!(budget.is_null("roll_over_start_date")? && budget.is_null("capacities")?);
    assert_eq!(
        (other.get::<String>("name")?, other.get::<String>("iso_currency_code")?, other.get::<Date>("roll_over_start_date")?.to_string()),
        ("Trips".to_string(), "EUR".to_string(), "2026-01-01".to_string())
    );
    let chart = insert(&db, "charts", data! {"user_id": ann, "type": "line"}, &["chart_id", "name"], false).await?.one()?;
    assert_eq!(chart.get::<String>("name")?, "Unnamed");
    // The charts of the group, narrowed by type or by id.
    let lines = db.query(find(Query::table("charts"), &Filters::new(Scope::Group(ann)).eq("type", "line"))).await?;
    assert_eq!(column::<Uuid>(&lines, "chart_id"), [chart.get::<Uuid>("chart_id")?]);
    assert!(db.query(find(Query::table("charts").key([chart.get::<Uuid>("chart_id")?]), &Filters::new(Scope::Group(ann)).eq("type", "bar"))).await?.is_empty());
    let split =
        insert(&db, "split_transactions", data! {"user_id": ann, "transaction_id": "manual-t1", "account_id": "manual-1", "date": "2026-03-01"}, &["*"], false)
            .await?
            .one()?;
    assert_eq!((split.get::<Decimal>("amount")?, split.get::<String>("custom_name")?), (dec("0"), String::new()));
    let key = insert(&db, "api_keys", data! {"user_id": ann, "name": "laptop", "key_hash": "ab".repeat(32), "key_prefix": "key_ab"}, &[], false).await?;
    assert_eq!(key.columns(), ["key_id"]);
    let key = stored(&db, "api_keys", key.one()?.get::<Uuid>("key_id")?).await?;
    assert!(key.get::<Vec<String>>("scopes")?.is_empty() && key.get::<Timestamp>("created_at")? >= before);
    // An id the caller does supply is kept.
    let chosen = insert(&db, "budgets", data! {"budget_id": uuid(7), "user_id": ann}, &[], false).await?.one()?;
    assert_eq!(chosen.get::<Uuid>("budget_id")?, uuid(7));

    // A key present with null is written as NULL, default or not.
    let unnamed = insert(&db, "budgets", data! {"user_id": ann, "name": Value::Null, "roll_over": Value::Null}, &["*"], false).await?.one()?;
    assert!(unnamed.is_null("name")? && unnamed.is_null("roll_over")?);

    // What cannot be stored is an error the caller reports as that row's
    // 500: a missing required column, a NULL where none may be, an owner
    // that does not exist, a key already there.
    let no_item = insert(&db, "accounts", data! {"user_id": ann, "account_id": "manual-2", "institution_id": "manual"}, &[], false).await.unwrap_err();
    assert_eq!(no_item.code, ErrorCode::NotNullViolation);
    assert!(no_item.violates("item_id"));
    assert_eq!(
        code(
            insert(&db, "transactions", data! {"user_id": ann, "transaction_id": "t-null-date", "account_id": "manual-1", "date": Value::Null}, &[], false)
                .await
        ),
        ErrorCode::NotNullViolation
    );
    assert_eq!(code(txn(&db, uuid(404), "t-orphan", "manual-1", 1, "2026-03-01").await), ErrorCode::ForeignKeyViolation);
    let twice = txn(&db, ann, "bank-t1", "manual-1", 6, "2026-03-02").await.unwrap_err();
    assert_eq!(twice.code, ErrorCode::UniqueViolation);
    assert!(twice.violates(PRIMARY));
    // A `VARCHAR(10)` is text with a check on its length.
    txn(&db, ann, "t-long", "manual-1", 1, "2026-03-01").await?;
    let too_long = update(&db, "transactions", "t-long", data! {"iso_currency_code": "US-DOLLARS!"}, Scope::Owner(ann), vec![]).await.unwrap_err();
    assert_eq!(too_long.code, ErrorCode::CheckViolation);
    assert!(too_long.violates("iso_currency_code_length"));
    assert_eq!(stored(&db, "transactions", "bank-t1").await?.get::<Decimal>("amount")?, dec("5"));
    assert_eq!(matching(&db, ann, val(true)).await?, ["bank-t1", "manual-t1", "t-long"]);
    Ok(())
}

// R-W5, R-S3, R-T9: the builder's insert with `ON CONFLICT DO NOTHING`: a row that collides on any unique key, a
// partial expression index included, is skipped in silence.
#[tokio::test(flavor = "multi_thread")]
async fn builder_insert_do_nothing_reports_any_taken_unique_key_as_no_row() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    // An identity provider's user, with the email when it has one.
    let create_sso_user = |username: &'static str, subject: &'static str, email: Option<&'static str>| {
        let mut row = data! {"username": username, "auth_sub": subject};
        row.extend(email.map(|email| ("email", Value::from(email))));
        insert(&db, "users", row, &["user_id", "username"], true)
    };
    let first = create_sso_user("ann", "sub-1", Some("Ann@Example.com")).await?;
    assert_eq!((first.affected(), first.len()), (1, 1));
    let ann: Uuid = first.one()?.get("user_id")?;

    // "No row" stands for a taken name, a taken subject, or a taken email
    // in any letter case. Nothing is counted, nothing is an error.
    for (username, subject, email) in [("ann", "sub-2", None), ("ann2", "sub-1", None), ("ann3", "sub-3", Some("ANN@example.COM"))] {
        let skipped = create_sso_user(username, subject, email).await?;
        assert_eq!((skipped.affected(), skipped.len(), status(&skipped)), (0, 0, 404), "{username}");
    }
    // The caller's second try, without the email, goes through.
    let third = create_sso_user("ann3", "sub-3", None).await?.one()?;
    assert_eq!(third.get::<String>("username")?, "ann3");
    // Users without an email do not collide with each other.
    create_sso_user("ann4", "sub-4", None).await?.one()?;
    assert_eq!(db.query(find(Query::table("users"), &Filters::new(Scope::Everyone))).await?.len(), 3);
    assert_eq!(stored(&db, "users", ann).await?.get::<String>("email")?, "Ann@Example.com");

    // Without the clause each of those collisions is an error that names
    // the index it hit.
    let plain = |row: Data| insert(&db, "users", row, &[], false);
    assert!(plain(data! {"username": "ann"}).await.unwrap_err().violates("users_username_key"));
    assert!(plain(data! {"username": "x1", "auth_sub": "sub-1"}).await.unwrap_err().violates("users_auth_sub_key"));
    let by_email = plain(data! {"username": "x2", "email": "ann@EXAMPLE.com"}).await.unwrap_err();
    assert_eq!(by_email.code, ErrorCode::UniqueViolation);
    assert!(by_email.violates("idx_users_lower_email"));

    // Inviting twice is a silent no-op, and the invitation is then read
    // back by its pair.
    let bob = user(&db, "bob").await?;
    let invite = |from: Uuid, to: Uuid| insert(&db, "sharing_invitations", data! {"inviter_user_id": from, "invitee_user_id": to}, &["invitation_id"], true);
    let invitation: Uuid = invite(ann, bob).await?.one()?.get("invitation_id")?;
    assert!(invite(ann, bob).await?.is_empty());
    let by_pair = db.query(Query::table("sharing_invitations").index("sharing_invitations_pair_unique", KeyRange::eq([ann, bob]))).await?;
    assert_eq!(column::<Uuid>(&by_pair, "invitation_id"), [invitation]);
    // The crossing invitation is another pair, and is found the same way.
    assert!(find_one(
        &db,
        Query::table("sharing_invitations").index("sharing_invitations_pair_unique", KeyRange::eq([bob, ann])),
        &Filters::new(Scope::Everyone)
    )
    .await?
    .is_none());
    assert_eq!(invite(bob, ann).await?.len(), 1);
    assert_eq!(db.query(Query::table("sharing_invitations")).await?.len(), 2);
    Ok(())
}

// R-W6, R-W7, R-Q8: the builder's update: 200 or 304 from the returned row, with the scope ANDed into the write.
#[tokio::test(flavor = "multi_thread")]
async fn builder_update_writes_only_supplied_columns_and_reports_missing_rows() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    for (id, owner) in [("t-alice", home.alice), ("t-bob", home.bob), ("t-carol", home.carol), ("t-gone", home.alice)] {
        insert(
            &db,
            "transactions",
            data! {
                "user_id": owner, "transaction_id": id, "account_id": "acc-1", "name": "COFFEE", "amount": 4.5, "date": "2026-03-05",
                "raw": json!({"id": id}),
            },
            &[],
            false,
        )
        .await?;
    }
    soft_delete(&db, "transactions", "t-gone", Scope::Owner(home.alice)).await?;
    let long_ago = ts("2001-01-01T00:00:00Z");
    for id in ["t-alice", "t-bob", "t-carol", "t-gone"] {
        backdate(&db, "transactions", id, long_ago).await?;
    }
    let before = db.now().await?;
    let viewer = Scope::Group(home.alice);

    // Alice labels Bob's transaction. Only the columns she sent change; the
    // row stays Bob's; `updated` moves to now.
    let outcome = update(&db, "transactions", "t-bob", data! {"label_memo": "dinner", "label_category_confidence": 1}, viewer, vec![]).await?;
    assert_eq!(update_status(&outcome), 200);
    let returned = outcome.expect("a statement was sent");
    assert_eq!((returned.columns(), returned.affected()), (&["transaction_id".to_string()][..], 1));
    assert_eq!(returned.one()?.get::<String>("transaction_id")?, "t-bob");
    let row = stored(&db, "transactions", "t-bob").await?;
    assert_eq!((row.get::<String>("label_memo")?, row.get::<f64>("label_category_confidence")?), ("dinner".to_string(), 1.0));
    assert_eq!((row.get::<String>("name")?, row.get::<Decimal>("amount")?, row.get::<Uuid>("user_id")?), ("COFFEE".to_string(), dec("4.5"), home.bob));
    assert!(row.get::<Timestamp>("updated")? >= before);

    // A key present with null clears the column; the others stay.
    update(&db, "transactions", "t-bob", data! {"label_memo": Value::Null, "name": "Dinner out"}, viewer, vec![]).await?;
    let row = stored(&db, "transactions", "t-bob").await?;
    assert!(row.is_null("label_memo")?);
    assert_eq!((row.get::<String>("name")?, row.get::<f64>("label_category_confidence")?), ("Dinner out".to_string(), 1.0));

    // "No row" is the answer for a row outside the group, a deleted row
    // and a row that never was. Nothing about them changes.
    for id in ["t-carol", "t-gone", "t-never"] {
        let outcome = update(&db, "transactions", id, data! {"name": "mine now"}, viewer, vec![]).await?;
        assert_eq!((update_status(&outcome), outcome.map(|rows| rows.len())), (304, Some(0)), "{id}");
    }
    let carols = stored(&db, "transactions", "t-carol").await?;
    assert_eq!((carols.get::<String>("name")?, carols.get::<Timestamp>("updated")?), ("COFFEE".to_string(), long_ago));
    assert_eq!(stored(&db, "transactions", "t-gone").await?.get::<Timestamp>("updated")?, long_ago);
    // The strict scope: a group member is not the owner.
    assert_eq!(update_status(&update(&db, "transactions", "t-bob", data! {"name": "x"}, Scope::Owner(home.alice), vec![]).await?), 304);

    // With nothing to set there is no statement at all: `raw` and the
    // key are never written by an update.
    assert!(update(&db, "transactions", "t-alice", data! {}, viewer, vec![]).await?.is_none());
    assert!(update(&db, "transactions", "t-alice", data! {"raw": json!({"forged": true}), "transaction_id": "t-renamed"}, viewer, vec![]).await?.is_none());
    update(&db, "transactions", "t-alice", data! {"raw": json!({"forged": true}), "label_memo": "kept"}, viewer, vec![]).await?;
    let row = stored(&db, "transactions", "t-alice").await?;
    assert_eq!((row.get::<Json>("raw")?, row.get::<String>("label_memo")?), (json!({"id": "t-alice"}), "kept".to_string()));

    // An item is updated within the group, or by the sync job with no
    // owner at all.
    upsert(&db, "items", data! {"item_id": "item-1", "user_id": home.bob, "status": "OK", "cursor": "c-1"}, None, None).await?;
    assert_eq!(update_status(&update(&db, "items", "item-1", data! {"status": "BAD", "status_reason": "LOGIN_REQUIRED"}, viewer, vec![]).await?), 200);
    assert_eq!(update_status(&update(&db, "items", "item-1", data! {"status": "OK"}, Scope::Group(home.carol), vec![]).await?), 304);
    assert_eq!(
        update_status(
            &update(
                &db,
                "items",
                "item-1",
                data! {"last_sync_status": "success", "last_sync_at": "2026-03-05T10:00:00Z", "last_sync_error": Value::Null},
                Scope::Everyone,
                vec![]
            )
            .await?
        ),
        200
    );
    let item = stored(&db, "items", "item-1").await?;
    assert_eq!(
        (item.get::<String>("status")?, item.get::<String>("cursor")?, item.get::<Timestamp>("last_sync_at")?),
        ("BAD".to_string(), "c-1".to_string(), ts("2026-03-05T10:00:00Z"))
    );

    // Any subset of the columns, JSON and dates included.
    let budget: Uuid = insert(&db, "budgets", data! {"user_id": home.alice, "name": "Home"}, &[], false).await?.one()?.get("budget_id")?;
    update(
        &db,
        "budgets",
        budget,
        data! {"roll_over": true, "roll_over_start_date": "2026-02-01", "capacities": json!([{"amount": 100}])},
        Scope::Group(home.bob),
        vec![],
    )
    .await?;
    let row = stored(&db, "budgets", budget).await?;
    assert_eq!(
        (row.get::<String>("name")?, row.get::<String>("iso_currency_code")?, row.get::<bool>("roll_over")?),
        ("Home".to_string(), "USD".to_string(), true)
    );
    assert_eq!((row.get::<Date>("roll_over_start_date")?.to_string(), row.get::<Json>("capacities")?), ("2026-02-01".to_string(), json!([{"amount": 100}])));
    Ok(())
}

/// The columns a conflicting item write may rewrite: never the owner.
const ITEM_COLUMNS: &[&str] = &[
    "access_token",
    "institution_id",
    "available_products",
    "cursor",
    "status",
    "status_reason",
    "provider",
    "last_sync_status",
    "last_sync_at",
    "last_sync_error",
    "raw",
];

/// The same for a holding: never the owner or the account.
const HOLDING_COLUMNS: &[&str] =
    &["security_id", "institution_price", "institution_price_as_of", "institution_value", "cost_basis", "quantity", "iso_currency_code", "raw"];

// R-W4, R-W1, R-T7: the builder's upsert, with and without an allowlist. Upserts carry no owner predicate; the
// allowlist is the protection.
#[tokio::test(flavor = "multi_thread")]
async fn builder_upsert_rewrites_supplied_columns_and_leaves_absent_ones() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let long_ago = ts("2001-01-01T00:00:00Z");

    // The first sync inserts and gets the whole row back.
    let pending = upsert(
        &db,
        "transactions",
        data! {
            "user_id": home.alice, "transaction_id": "p-1", "account_id": "acc-1", "name": "BLUE BOTTLE #12", "merchant_name": "Blue Bottle",
            "amount": 4.5, "iso_currency_code": "USD", "date": "2026-03-05", "pending": true, "payment_channel": "in store", "raw": json!({"v": 1}),
        },
        None,
        None,
    )
    .await?;
    assert_eq!((pending.affected(), pending.columns().len()), (1, all_columns("transactions").len()));
    assert_eq!(pending.one()?.get::<String>("source")?, "bank");
    // The user labels it; then the provider reports it again, settled.
    update(
        &db,
        "transactions",
        "p-1",
        data! {"label_category_id": uuid(50), "label_category_confidence": 1, "label_memo": "coffee"},
        Scope::Owner(home.alice),
        vec![],
    )
    .await?;
    backdate(&db, "transactions", "p-1", long_ago).await?;
    let settled = upsert(
        &db,
        "transactions",
        data! {
            "user_id": home.alice, "transaction_id": "p-1", "account_id": "acc-1", "name": Value::Null, "merchant_name": "Blue Bottle Coffee",
            "amount": 4.75, "date": "2026-03-06", "pending": false, "raw": json!({"v": 2}),
        },
        None,
        None,
    )
    .await?;
    assert_eq!((settled.affected(), settled.len()), (1, 1));
    let row = settled.one()?;
    // Supplied columns are the proposed row's, a supplied null included.
    assert_eq!(
        (row.get::<Decimal>("amount")?, row.get::<Option<bool>>("pending")?, row.get::<Date>("date")?.to_string()),
        (dec("4.75"), Some(false), "2026-03-06".to_string())
    );
    assert!(row.is_null("name")?);
    assert_eq!((row.get::<String>("merchant_name")?, row.get::<Json>("raw")?), ("Blue Bottle Coffee".to_string(), json!({"v": 2})));
    // Columns the statement did not name keep what they had: the label,
    // and what the first write alone supplied.
    assert_eq!(
        (row.get::<Uuid>("label_category_id")?, row.get::<f64>("label_category_confidence")?, row.get::<String>("label_memo")?),
        (uuid(50), 1.0, "coffee".to_string())
    );
    assert_eq!((row.get::<String>("payment_channel")?, row.get::<String>("iso_currency_code")?), ("in store".to_string(), "USD".to_string()));
    assert!(row.get::<Timestamp>("updated")? > long_ago);
    assert_eq!(matching(&db, home.alice, val(true)).await?, ["p-1"]);

    // Every supplied column is rewritten, `user_id` among them: a
    // provider id that turns up under another owner moves the row.
    upsert(&db, "transactions", data! {"user_id": home.bob, "transaction_id": "p-1", "account_id": "acc-9", "date": "2026-03-06"}, None, None).await?;
    assert_eq!(stored(&db, "transactions", "p-1").await?.get::<Uuid>("user_id")?, home.bob);

    // The seeded users. Without an id the row is always new, so the caller
    // looks the name up first and upserts with the id it found.
    let admin: Uuid = upsert(&db, "users", data! {"username": "admin", "password": "hash-1"}, None, None).await?.one()?.get("user_id")?;
    let again = upsert(&db, "users", data! {"user_id": admin, "username": "admin", "password": "hash-2"}, None, None).await?.one()?;
    assert_eq!((again.get::<Uuid>("user_id")?, again.get::<String>("password")?), (admin, "hash-2".to_string()));
    let unseen = upsert(&db, "users", data! {"username": "admin", "password": "hash-3"}, None, None).await.unwrap_err();
    assert!(unseen.violates("users_username_key"));
    assert_eq!(stored(&db, "users", admin).await?.get::<String>("password")?, "hash-2");

    // An item write on a taken id may rewrite the allowlisted columns it
    // supplies, and nothing else. The owner is not one of them.
    let products = vec!["transactions", "investments"];
    upsert(
        &db,
        "items",
        data! {
            "item_id": "item-1", "user_id": home.alice, "access_token": "access-1", "institution_id": "ins-1", "available_products": products.clone(),
            "status": "OK", "provider": "bank",
        },
        Some(ITEM_COLUMNS),
        None,
    )
    .await?;
    let taken = upsert(&db, "items", data! {"item_id": "item-1", "user_id": home.carol, "access_token": "access-2", "status": "BAD"}, Some(ITEM_COLUMNS), None)
        .await?
        .one()?;
    assert_eq!(
        (taken.get::<Uuid>("user_id")?, taken.get::<String>("access_token")?, taken.get::<String>("status")?),
        (home.alice, "access-2".to_string(), "BAD".to_string())
    );
    assert_eq!(
        (taken.get::<Vec<String>>("available_products")?, taken.get::<String>("provider")?),
        (vec!["transactions".to_string(), "investments".to_string()], "bank".to_string())
    );
    // The bank sync advances the cursor with a write of just that; `raw` is
    // on the list, so the partial object replaces the payload.
    let advanced = upsert(
        &db,
        "items",
        data! {"item_id": "item-1", "user_id": home.alice, "cursor": "cursor-2", "raw": json!({"item_id": "item-1", "cursor": "cursor-2"})},
        Some(ITEM_COLUMNS),
        None,
    )
    .await?
    .one()?;
    assert_eq!(
        (advanced.get::<String>("cursor")?, advanced.get::<String>("access_token")?, advanced.get::<Json>("raw")?),
        ("cursor-2".to_string(), "access-2".to_string(), json!({"item_id": "item-1", "cursor": "cursor-2"}))
    );

    // Neither the owner nor the account of a holding moves.
    upsert(
        &db,
        "holdings",
        data! {"user_id": home.alice, "holding_id": "h-1", "account_id": "acc-1", "security_id": "sec-1", "quantity": 2, "institution_value": 100},
        Some(HOLDING_COLUMNS),
        None,
    )
    .await?;
    let holding = upsert(
        &db,
        "holdings",
        data! {"user_id": home.carol, "holding_id": "h-1", "account_id": "acc-x", "security_id": "sec-2", "quantity": 3.5},
        Some(HOLDING_COLUMNS),
        None,
    )
    .await?
    .one()?;
    assert_eq!(
        (holding.get::<Uuid>("user_id")?, holding.get::<String>("account_id")?, holding.get::<String>("security_id")?),
        (home.alice, "acc-1".to_string(), "sec-2".to_string())
    );
    assert_eq!((holding.get::<Decimal>("quantity")?, holding.get::<Decimal>("institution_value")?), (dec("3.5"), dec("100")));
    // When the list leaves nothing to rewrite, the conflict is left alone
    // and no row comes back; a new key is still inserted.
    // (The proposed row must still be a whole one: as in Postgres, what a
    // row must hold is checked before the conflict is looked for.)
    let whole = data! {"user_id": home.carol, "holding_id": "h-1", "account_id": "acc-x", "security_id": "sec-9"};
    let untouched = upsert(&db, "holdings", whole, Some(&["quantity"]), None).await?;
    assert_eq!((untouched.affected(), untouched.len()), (0, 0));
    let fresh_key =
        upsert(&db, "holdings", data! {"user_id": home.carol, "holding_id": "h-2", "account_id": "acc-x", "security_id": "sec-1"}, Some(&[]), None).await?;
    assert_eq!(fresh_key.one()?.get::<Uuid>("user_id")?, home.carol);

    // A global table: every supplied column, once per security.
    upsert(
        &db,
        "securities",
        data! {
            "security_id": "sec-1", "name": "Total Market", "ticker_symbol": "VTI", "type": "etf", "close_price": 250.1,
            "close_price_as_of": "2026-03-04", "raw": json!({"sector": "n/a", "is_cash_equivalent": false}),
        },
        None,
        None,
    )
    .await?;
    let repriced =
        upsert(&db, "securities", data! {"security_id": "sec-1", "close_price": 251.25, "close_price_as_of": "2026-03-05"}, None, None).await?.one()?;
    assert_eq!(
        (repriced.get::<String>("ticker_symbol")?, repriced.get::<Decimal>("close_price")?, repriced.get::<Date>("close_price_as_of")?.to_string()),
        ("VTI".to_string(), dec("251.25"), "2026-03-05".to_string())
    );
    assert_eq!(repriced.get::<Json>("raw")?["is_cash_equivalent"], json!(false));
    Ok(())
}

// R-W2, R-W4, R-W5, R-Q4: the builder's multi-row upsert: rows de-duplicated by key and grouped by the columns they
// supply; with an empty allowlist it only ever inserts.
#[tokio::test(flavor = "multi_thread")]
async fn builder_upsert_many_dedupes_rows_and_groups_them_by_columns() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let long_ago = ts("2001-01-01T00:00:00Z");

    // One multi-row statement. A key given twice is written once, with the
    // last row's values; the returned rows tell the caller which inputs
    // were stored.
    let returned = upsert_many(
        &db,
        "institutions",
        vec![
            data! {"institution_id": "ins-1", "name": "First", "raw": json!({"n": 1})},
            data! {"institution_id": "ins-2", "name": "Second", "raw": json!({"n": 2})},
            data! {"institution_id": "ins-1", "name": "First National", "raw": json!({"n": 3})},
        ],
        None,
    )
    .await?;
    assert_eq!(returned.iter().map(|row| row.get("institution_id")).collect::<DbResult<Vec<String>>>()?, ["ins-1", "ins-2"]);
    assert_eq!(returned[0].get::<String>("name")?, "First National");
    // Rows that supply different columns go in separate statements, so a
    // column one row leaves out is left alone for that row only.
    upsert_many(
        &db,
        "institutions",
        vec![
            data! {"institution_id": "ins-1", "name": "1st"},
            data! {"institution_id": "ins-3", "name": "Third", "raw": json!({"n": 4})},
            data! {"institution_id": "ins-2", "name": "2nd"},
        ],
        None,
    )
    .await?;
    let all = db.query(Query::table("institutions")).await?;
    assert_eq!(texts_of(&all, "name"), ["1st", "2nd", "Third"]);
    assert_eq!(column::<Json>(&all, "raw"), [json!({"n": 3}), json!({"n": 2}), json!({"n": 4})]);
    assert!(upsert_many(&db, "institutions", Vec::new(), None).await?.is_empty());

    // The monthly index values. `updated` moves on every upsert, changed or
    // not: the refresh job reads it to tell how long ago it last ran.
    let months = |values: [f64; 3]| {
        ["2026-03", "2026-01", "2026-02"]
            .iter()
            .zip(values)
            .map(|(month, value)| {
                data! {
                    "cpi_snapshot_id": format!("CUUR0000SA0-{month}"), "series_id": "CUUR0000SA0", "snapshot_date": format!("{month}-01"),
                    "index_value": value,
                }
            })
            .collect::<Vec<Data>>()
    };
    assert_eq!(upsert_many(&db, "cpi_snapshots", months([327.1, 325.252, 326.0]), None).await?.len(), 3);
    upsert_many(
        &db,
        "cpi_snapshots",
        vec![data! {"cpi_snapshot_id": "OTHER-2026-01", "series_id": "OTHER", "snapshot_date": "2026-01-01", "index_value": 1}],
        None,
    )
    .await?;
    for month in ["2026-01", "2026-02", "2026-03"] {
        backdate(&db, "cpi_snapshots", format!("CUUR0000SA0-{month}"), long_ago).await?;
    }
    let again = upsert_many(&db, "cpi_snapshots", months([327.4, 325.252, 326.0]), None).await?;
    assert!(again.iter().all(|row| row.get::<Timestamp>("updated").unwrap() > long_ago));
    // A whole series, oldest first, straight off its index.
    let series = db.query(Query::table("cpi_snapshots").index("idx_cpi_snapshots_series_id_snapshot_date", KeyRange::eq(["CUUR0000SA0"]))).await?;
    assert_eq!(column::<Date>(&series, "snapshot_date").iter().map(Date::to_string).collect::<Vec<_>>(), ["2026-01-01", "2026-02-01", "2026-03-01"]);
    assert_eq!(column::<Decimal>(&series, "index_value"), [dec("325.252"), dec("326"), dec("327.4")]);

    // A plan statement imported again never overwrites. A row whose id
    // exists, even under another owner, is skipped in silence, and only the
    // rows actually inserted come back.
    let trade = |id: &'static str, owner: Uuid, amount: f64| {
        data! {
            "user_id": owner, "investment_transaction_id": id, "account_id": "acc-401k", "date": "2026-03-05", "amount": amount, "source": "plan",
        }
    };
    assert_eq!(
        upsert_many(&db, "investment_transactions", vec![trade("plan-1", home.alice, 10.0), trade("plan-2", home.carol, 20.0)], Some(&[])).await?.len(),
        2
    );
    let imported = upsert_many(
        &db,
        "investment_transactions",
        vec![trade("plan-1", home.alice, 11.0), trade("plan-2", home.alice, 21.0), trade("plan-3", home.alice, 30.0)],
        Some(&[]),
    )
    .await?;
    assert_eq!(imported.iter().map(|row| row.get("investment_transaction_id")).collect::<DbResult<Vec<String>>>()?, ["plan-3"]);
    let kept = stored(&db, "investment_transactions", "plan-2").await?;
    assert_eq!((kept.get::<Uuid>("user_id")?, kept.get::<Decimal>("amount")?), (home.carol, dec("20")));
    // Which of the ids an owner already has, deleted or not.
    soft_delete(&db, "investment_transactions", "plan-1", Scope::Owner(home.alice)).await?;
    let present = db
        .query(find(
            Query::table("investment_transactions").keys([["plan-1"], ["plan-2"], ["plan-3"], ["plan-4"]]),
            &Filters::new(Scope::Owner(home.alice)).with_tombstones(),
        ))
        .await?;
    assert_eq!(texts_of(&present, "investment_transaction_id"), ["plan-1", "plan-3"]);
    // One of them by id, if it is live and the group's.
    let one = |id: &'static str, viewer: Uuid| {
        let db = &db;
        async move { find_one(db, Query::table("investment_transactions").key([id]), &Filters::new(Scope::Group(viewer))).await }
    };
    assert_eq!(one("plan-3", home.bob).await?.expect("a group member's row").get::<Decimal>("amount")?, dec("30"));
    assert!(one("plan-3", home.carol).await?.is_none() && one("plan-1", home.bob).await?.is_none());

    // Both users join a group unless they are in one already.
    let dave = user(&db, "dave").await?;
    let other_group = uuid(0x6008);
    let joined = upsert_many(
        &db,
        "sharing_members",
        vec![data! {"user_id": home.bob, "group_id": other_group}, data! {"user_id": dave, "group_id": other_group}],
        Some(&[]),
    )
    .await?;
    assert_eq!(joined.iter().map(|row| row.get("user_id")).collect::<DbResult<Vec<Uuid>>>()?, [dave]);
    assert_eq!(stored(&db, "sharing_members", home.bob).await?.get::<Uuid>("group_id")?, home.group);
    Ok(())
}

// R-W6, R-W10, R-Q10, R-Q8: the builder's soft deletes: of one row, of some keys, by a column, and by a column of a
// related table.
#[tokio::test(flavor = "multi_thread")]
async fn builder_soft_deletes_keep_tombstones_and_report_counts() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let long_ago = ts("2001-01-01T00:00:00Z");
    for (id, owner, account) in [
        ("t-a1", home.alice, "acc-a"),
        ("t-a2", home.alice, "acc-a"),
        ("t-a3", home.alice, "acc-a2"),
        ("t-b1", home.bob, "acc-b"),
        ("t-b2", home.bob, "acc-b"),
        ("t-c1", home.carol, "acc-c"),
        ("t-c2", home.carol, "acc-c"),
    ] {
        txn(&db, owner, id, account, 10, "2026-03-05").await?;
        backdate(&db, "transactions", id, long_ago).await?;
    }
    let flags = |ids: &'static [&'static str]| {
        let db = &db;
        async move {
            Ok::<_, DbError>(column::<Option<bool>>(
                &db.query(Query::table("transactions").keys(ids.iter().map(|id| [*id])).columns(["is_deleted"])).await?,
                "is_deleted",
            ))
        }
    };

    // One row, within the group; the answer is a boolean.
    let chart: Uuid = insert(&db, "charts", data! {"user_id": home.alice, "name": "Worth"}, &[], false).await?.one()?.get("chart_id")?;
    assert!(!soft_delete(&db, "charts", chart, Scope::Group(home.carol)).await?);
    assert!(!soft_delete(&db, "charts", uuid(404), Scope::Group(home.alice)).await?);
    assert!(soft_delete(&db, "charts", chart, Scope::Group(home.bob)).await?);
    assert_eq!(stored(&db, "charts", chart).await?.get::<Option<bool>>("is_deleted")?, Some(true));
    // The row stays, hidden from live reads and shown as a tombstone.
    assert!(db.query(find(Query::table("charts"), &Filters::new(Scope::Group(home.alice)))).await?.is_empty());
    assert_eq!(db.query(find(Query::table("charts"), &Filters::new(Scope::Group(home.alice)).with_tombstones())).await?.len(), 1);
    // There is no `LIVE` guard: deleting it again succeeds and stamps it
    // again, so the tombstone shows up in the next delta too.
    backdate(&db, "charts", chart, long_ago).await?;
    assert!(soft_delete(&db, "charts", chart, Scope::Group(home.alice)).await?);
    assert!(stored(&db, "charts", chart).await?.get::<Timestamp>("updated")? > long_ago);
    // A user is deleted with no scope at all.
    assert!(soft_delete(&db, "users", user(&db, "dave").await?, Scope::Everyone).await?);

    // By keys: the count of the ones that were the group's.
    let asked: Vec<Value> = ["t-a1", "t-b1", "t-c1", "t-zz"].map(Value::from).to_vec();
    assert_eq!(bulk_soft_delete(&db, "transactions", asked, Scope::Group(home.alice)).await?, 2);
    assert_eq!(flags(&["t-a1", "t-b1", "t-c1"]).await?, [Some(true), Some(true), Some(false)]);
    assert_eq!(bulk_soft_delete(&db, "transactions", Vec::new(), Scope::Group(home.alice)).await?, 0);
    assert!(stored(&db, "transactions", "t-a1").await?.get::<Timestamp>("updated")? > long_ago);
    assert_eq!(stored(&db, "transactions", "t-c1").await?.get::<Timestamp>("updated")?, long_ago);

    // By a column, with one value: every row of the account, the ones
    // already deleted counted again.
    assert_eq!(bulk_soft_delete_by_column(&db, "transactions", "account_id", "acc-a", Scope::Group(home.bob)).await?, 2);
    assert_eq!(flags(&["t-a1", "t-a2", "t-a3"]).await?, [Some(true), Some(true), Some(false)]);
    // With a list, as the item cascade has it: any of the accounts. The
    // scope still holds, so Carol's account in the list changes nothing.
    assert_eq!(bulk_soft_delete_by_column(&db, "transactions", "account_id", vec!["acc-a2", "acc-b", "acc-c"], Scope::Group(home.alice)).await?, 3);
    assert_eq!(flags(&["t-a3", "t-b1", "t-b2", "t-c1", "t-c2"]).await?, [Some(true), Some(true), Some(true), Some(false), Some(false)]);
    // An empty list sends no statement.
    assert_eq!(bulk_soft_delete_by_column(&db, "transactions", "account_id", Vec::<String>::new(), Scope::Group(home.carol)).await?, 0);
    assert_eq!(flags(&["t-c1", "t-c2"]).await?, [Some(false), Some(false)]);

    // By a column on the pair table, once per side; then by a column of a
    // related table: pairs whose transaction sits on one of some accounts.
    // The subquery has no `LIVE`: it runs right after those transactions
    // were deleted.
    for (n, a, b, owner) in [
        (1, "t-a1", "t-b1", home.alice),
        (2, "t-a2", "t-a3", home.alice),
        (3, "t-b2", "t-c1", home.bob),
        (4, "t-c1", "t-c2", home.carol),
        (5, "t-a3", "t-c2", home.carol),
    ] {
        db.insert(
            Insert::into("transaction_pairs")
                .set("pair_id", uuid(n))
                .set("user_id", owner)
                .set("transaction_id_a", a)
                .set("transaction_id_b", b)
                .set("status", "suggested"),
        )
        .await?;
    }
    let pair_flags =
        || async { Ok::<_, DbError>(column::<Option<bool>>(&db.query(Query::table("transaction_pairs").columns(["is_deleted"])).await?, "is_deleted")) };
    assert_eq!(bulk_soft_delete_by_column(&db, "transaction_pairs", "transaction_id_a", vec!["t-a1", "t-c1"], Scope::Group(home.alice)).await?, 1);
    assert_eq!(bulk_soft_delete_by_column(&db, "transaction_pairs", "transaction_id_b", vec!["t-a1", "t-c1"], Scope::Group(home.alice)).await?, 1);
    assert_eq!(pair_flags().await?, [Some(true), Some(false), Some(true), Some(false), Some(false)]);
    let on_accounts: Vec<Value> = vec!["acc-a2".into(), "acc-c".into()];
    assert_eq!(
        bulk_soft_delete_by_related_column(
            &db,
            "transaction_pairs",
            "transaction_id_b",
            ("transactions", "transaction_id", "account_id"),
            on_accounts.clone(),
            Scope::Group(home.alice)
        )
        .await?,
        2
    );
    assert_eq!(
        bulk_soft_delete_by_related_column(
            &db,
            "transaction_pairs",
            "transaction_id_a",
            ("transactions", "transaction_id", "account_id"),
            on_accounts,
            Scope::Group(home.alice)
        )
        .await?,
        0
    );
    // Carol's pairs are out of the group's reach either way.
    assert_eq!(pair_flags().await?, [Some(true), Some(true), Some(true), Some(false), Some(false)]);
    Ok(())
}

// R-Q1, R-S3, R-T9, R-W7, R-X3: users are found by id, name and subject, linked to an identity by compare-and-swap,
// listed and soft-deleted.
#[tokio::test(flavor = "multi_thread")]
async fn users_are_found_by_key_name_and_subject_and_linked_to_an_identity_once() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let bob = user(&db, "bob").await?;
    let cat = user(&db, "cat").await?;
    let everyone = Filters::new(Scope::Everyone);
    let by_name = |name: &str| Query::table("users").index("users_username_key", KeyRange::eq([name]));
    let by_subject = |subject: &str| Query::table("users").index("users_auth_sub_key", KeyRange::eq([subject]));

    // By id, by name, or by both. Names are case-sensitive.
    assert_eq!(find_one(&db, by_name("ann"), &everyone).await?.expect("ann").get::<Uuid>("user_id")?, ann);
    assert!(find_one(&db, by_name("Ann"), &everyone).await?.is_none());
    assert!(find_one(&db, Query::table("users").key([ann]), &everyone.clone().eq("username", "ann")).await?.is_some());
    assert!(find_one(&db, Query::table("users").key([ann]), &everyone.clone().eq("username", "bob")).await?.is_none());

    // An account is linked to an identity only while it has none. The row
    // comes back to the sign-in that linked it and to no other.
    let link = |user: Uuid, subject: &'static str| update(&db, "users", user, data! {"auth_sub": subject}, Scope::Everyone, vec![col("auth_sub").is_null()]);
    let linked = link(ann, "sub-ann").await?.expect("a statement");
    assert_eq!(linked.one()?.get::<Uuid>("user_id")?, ann);
    assert_eq!(update_status(&link(ann, "sub-other").await?), 304);
    assert_eq!(stored(&db, "users", ann).await?.get::<String>("auth_sub")?, "sub-ann");
    // Every single-sign-on sign-in finds its user by the subject.
    assert_eq!(find_one(&db, by_subject("sub-ann"), &everyone).await?.expect("linked").get::<String>("username")?, "ann");
    assert!(find_one(&db, by_subject("sub-other"), &everyone).await?.is_none());

    // Two sign-ins racing to link the same account: one wins.
    let (first, second) = tokio::join!(link(bob, "sub-b1"), link(bob, "sub-b2"));
    let outcomes = [update_status(&first?), update_status(&second?)];
    assert!(outcomes == [200, 304] || outcomes == [304, 200], "{outcomes:?}");
    let winner = if outcomes[0] == 200 { "sub-b1" } else { "sub-b2" };
    assert_eq!(stored(&db, "users", bob).await?.get::<String>("auth_sub")?, winner);

    // A subject belongs to one user; having none is not a subject.
    let taken = link(cat, "sub-ann").await.unwrap_err();
    assert_eq!(taken.code, ErrorCode::UniqueViolation);
    assert!(taken.violates("users_auth_sub_key"));
    assert!(stored(&db, "users", cat).await?.is_null("auth_sub")?);
    user(&db, "dan").await?;

    // Every live user, for the hourly jobs. A soft delete hides one, which
    // still holds its name.
    assert!(soft_delete(&db, "users", cat, Scope::Everyone).await?);
    let live_users = db.query(find(Query::table("users"), &everyone)).await?;
    let mut names = texts_of(&live_users, "username");
    names.sort();
    assert_eq!(names, ["ann", "bob", "dan"]);
    assert!(find_one(&db, by_name("cat"), &everyone).await?.is_none());
    assert!(find_one(&db, by_name("cat"), &everyone.clone().with_tombstones()).await?.is_some());
    assert!(insert(&db, "users", data! {"username": "cat"}, &[], false).await.unwrap_err().violates("users_username_key"));
    Ok(())
}

// R-W4, R-W7, R-W10, R-Q1, R-T9: a session through the builder: its upsert, read and update, and the hard deletes
// by key, by keys, by a column and by a condition.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_are_upserted_refreshed_without_resurrection_and_hard_deleted() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let long_ago = ts("2001-01-01T00:00:00Z");
    let week = 604_800_000_i64;

    // The first write of a single-sign-on session names every column, the
    // `auth_*` ones too.
    let created = upsert(
        &db,
        "sessions",
        data! {
            "session_id": "s-sso", "user_user_id": ann, "user_username": "ann", "cookie_original_max_age": week, "cookie_max_age": week,
            "cookie_signed": true, "cookie_expires": "2026-03-12T10:00:00Z", "cookie_http_only": true, "cookie_path": "/",
            "cookie_domain": Value::Null, "cookie_secure": false, "cookie_same_site": "lax",
            "auth_sid": "sid-1", "auth_refresh_token": "refresh-1", "auth_recheck_at": "2026-03-05T10:10:00Z",
        },
        None,
        None,
    )
    .await?
    .one()?;
    let created_at: Timestamp = created.get("created_at")?;
    backdate(&db, "sessions", "s-sso", long_ago).await?;

    // The write at the end of a later request does not name them, and so
    // leaves them as they are. `created_at` is only ever its default.
    let written = upsert(
        &db,
        "sessions",
        data! {"session_id": "s-sso", "user_user_id": ann, "user_username": "ann", "cookie_max_age": week - 5_000, "cookie_expires": "2026-03-12T10:00:05Z"},
        None,
        None,
    )
    .await?
    .one()?;
    assert_eq!(
        (written.get::<String>("auth_sid")?, written.get::<String>("auth_refresh_token")?, written.get::<Timestamp>("auth_recheck_at")?),
        ("sid-1".to_string(), "refresh-1".to_string(), ts("2026-03-05T10:10:00Z"))
    );
    assert_eq!(
        (written.get::<i64>("cookie_max_age")?, written.get::<i64>("cookie_original_max_age")?, written.get::<Timestamp>("cookie_expires")?),
        (week - 5_000, week, ts("2026-03-12T10:00:05Z"))
    );
    assert_eq!(written.get::<Timestamp>("created_at")?, created_at);
    assert!(written.get::<Timestamp>("updated")? > long_ago);
    // A column named with null is cleared.
    upsert(&db, "sessions", data! {"session_id": "s-sso", "auth_refresh_token": Value::Null}, None, None).await?;
    // The session is read on every request that carries the cookie.
    let read = find_one(&db, Query::table("sessions").key(["s-sso"]), &Filters::new(Scope::Everyone)).await?.expect("the session");
    assert!(read.is_null("auth_refresh_token")? && read.is_null("cookie_domain")?);
    assert_eq!(
        (read.get::<String>("auth_sid")?, read.get::<Uuid>("user_user_id")?, read.get::<f64>("cookie_max_age")?),
        ("sid-1".to_string(), ann, (week - 5_000) as f64)
    );

    // The re-check stores its result and learns whether the row was there.
    let rechecked = update(
        &db,
        "sessions",
        "s-sso",
        data! {"auth_sid": "sid-2", "auth_refresh_token": "refresh-2", "auth_recheck_at": "2026-03-05T10:20:00Z"},
        Scope::Everyone,
        vec![],
    )
    .await?;
    assert_eq!(update_status(&rechecked), 200);

    // Sign-out deletes the row for good; again finds nothing.
    assert!(hard_delete(&db, "sessions", "s-sso").await?);
    assert!(!hard_delete(&db, "sessions", "s-sso").await?);
    assert!(db.get("sessions", ["s-sso"]).await?.is_none());
    // A refresh at the end of a request that began before the deletion is
    // an update, so the session stays deleted. The upsert that first wrote
    // it would have brought it back.
    let refreshed =
        update(&db, "sessions", "s-sso", data! {"user_user_id": ann, "user_username": "ann", "cookie_max_age": week}, Scope::Everyone, vec![]).await?;
    assert_eq!(update_status(&refreshed), 304);
    assert!(db.get("sessions", ["s-sso"]).await?.is_none());

    // An identity-provider event ends sessions by its session id, or all of
    // a user's. Password sessions have no such id, and a missing one in the
    // event must not match them.
    let bob = user(&db, "bob").await?;
    for (id, owner, sid, expires) in [
        ("s-1", ann, Some("sid-a"), Some("2026-03-01T00:00:00Z")),
        ("s-2", ann, Some("sid-a"), Some("2026-03-10T00:00:00Z")),
        ("s-3", ann, None, Some("2026-03-05T00:00:00Z")),
        ("s-4", bob, Some("sid-b"), Some("2026-03-05T00:00:00.000001Z")),
        ("s-5", bob, None, None),
        ("s-6", bob, None, Some("2026-03-04T23:59:59Z")),
    ] {
        upsert(&db, "sessions", data! {"session_id": id, "user_user_id": owner, "auth_sid": sid, "cookie_expires": expires}, None, None).await?;
    }
    assert_eq!(hard_delete_by_column(&db, "sessions", "auth_sid", Value::Null).await?, 0);
    assert!(db.query(Query::table("sessions").index("idx_sessions_auth_sid", KeyRange::eq([Value::Null]))).await?.is_empty());
    assert_eq!(hard_delete_by_column(&db, "sessions", "auth_sid", "sid-a").await?, 2);
    assert_eq!(hard_delete_by_column(&db, "sessions", "auth_sid", "sid-a").await?, 0);
    assert_eq!(texts_of(&db.query(Query::table("sessions").index("idx_sessions_user_user_id", KeyRange::eq([ann]))).await?, "session_id"), ["s-3"]);

    // The hourly purge, up to and including the caller's clock. A session
    // with no expiry is not "expired".
    assert_eq!(delete_by_condition(&db, "sessions", "cookie_expires", CmpOp::Le, "2026-03-05T00:00:00Z").await?, 2);
    assert_eq!(texts_of(&db.query(Query::table("sessions")).await?, "session_id"), ["s-4", "s-5"]);
    assert_eq!(hard_delete_by_column(&db, "sessions", "user_user_id", bob).await?, 2);
    // Several keys at once; the count is of the rows that existed.
    upsert(&db, "sessions", data! {"session_id": "s-7"}, None, None).await?;
    assert_eq!(bulk_hard_delete(&db, "sessions", vec!["s-7".into(), "s-8".into()]).await?, 1);
    assert!(db.query(Query::table("sessions")).await?.is_empty());
    Ok(())
}

// R-W1, R-W7, R-Q1, R-Q4, R-T7, R-S3: API keys are created, listed newest first, found by their hash, stamped when
// used and revoked once.
#[tokio::test(flavor = "multi_thread")]
async fn api_keys_are_listed_newest_first_and_revoked_once() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    let hash = |seed: &str| seed.repeat(64);

    // The id is the server's; `scopes` is a list of text.
    let mut keys = Vec::new();
    for (name, seed, owner, created) in [
        ("laptop", "a", ann, "2026-03-01T08:00:00Z"),
        ("phone", "b", ann, "2026-03-03T08:00:00Z"),
        ("script", "c", ann, "2026-03-02T08:00:00Z"),
        ("bobs", "d", bob, "2026-03-04T08:00:00Z"),
    ] {
        let row = data! {
            "user_id": owner, "name": name, "key_hash": hash(seed), "key_prefix": format!("key_{seed}"),
            "scopes": vec!["transactions:read", "accounts:read"], "expires_at": Value::Null, "created_at": created,
        };
        keys.push(insert(&db, "api_keys", row, &["key_id"], false).await?.one()?.get::<Uuid>("key_id")?);
    }
    let (laptop, phone) = (keys[0], keys[1]);
    assert_eq!(stored(&db, "api_keys", laptop).await?.get::<Vec<String>>("scopes")?, ["transactions:read", "accounts:read"]);
    // The hash is unique: the same key cannot be stored twice.
    let twice = insert(&db, "api_keys", data! {"user_id": bob, "name": "copy", "key_hash": hash("a"), "key_prefix": "key_a"}, &[], false).await.unwrap_err();
    assert!(twice.violates("api_keys_key_hash_key"));

    // The caller's own unrevoked keys, newest first, without the hash. Keys
    // are never shared, so the scope is the owner alone.
    let shown = ["key_id", "user_id", "name", "key_prefix", "scopes", "created_at", "last_used_at", "revoked_at", "expires_at"];
    let list = |owner: Uuid| {
        Query::table("api_keys").index("idx_api_keys_user_id_created_at", KeyRange::eq([owner])).reverse().filter(col("revoked_at").is_null()).columns(shown)
    };
    let listed = db.query(list(ann)).await?;
    assert_eq!(listed.columns(), shown);
    assert_eq!(texts_of(&listed, "name"), ["phone", "script", "laptop"]);

    // Every bearer request looks its key up by the hash.
    let by_hash = |seed: &str| Query::table("api_keys").index("api_keys_key_hash_key", KeyRange::eq([hash(seed)]));
    let found = find_one(&db, by_hash("b"), &Filters::new(Scope::Everyone)).await?.expect("the key");
    assert_eq!((found.get::<Uuid>("key_id")?, found.get::<Uuid>("user_id")?), (phone, ann));
    assert!(found.is_null("revoked_at")? && found.is_null("last_used_at")?);
    assert!(find_one(&db, by_hash("z"), &Filters::new(Scope::Everyone)).await?.is_none());
    // The "last used" stamp, by key alone.
    assert_eq!(update_status(&update(&db, "api_keys", phone, data! {"last_used_at": "2026-03-05T12:00:00Z"}, Scope::Everyone, vec![]).await?), 200);

    // A key is revoked once, by its owner. Anyone else, and a second
    // revoke, get no row, which the caller answers with "not found".
    let revoke = |key: Uuid, owner: Uuid| {
        update(&db, "api_keys", key, data! {"revoked_at": "2026-03-05T13:00:00Z"}, Scope::Owner(owner), vec![col("revoked_at").is_null()])
    };
    assert_eq!(update_status(&revoke(phone, bob).await?), 304);
    assert_eq!(update_status(&revoke(phone, ann).await?), 200);
    assert_eq!(update_status(&revoke(phone, ann).await?), 304);
    let revoked = stored(&db, "api_keys", phone).await?;
    assert_eq!((revoked.get::<Timestamp>("revoked_at")?, revoked.get::<Timestamp>("last_used_at")?), (ts("2026-03-05T13:00:00Z"), ts("2026-03-05T12:00:00Z")));
    assert_eq!(texts_of(&db.query(list(ann)).await?, "name"), ["script", "laptop"]);
    assert_eq!(texts_of(&db.query(list(bob)).await?, "name"), ["bobs"]);
    // The row is kept: a revoked key is still found by its hash.
    assert!(find_one(&db, by_hash("b"), &Filters::new(Scope::Everyone)).await?.is_some());
    Ok(())
}

// R-X7, R-Q8, R-X4, R-X1: `CURRENT_TIMESTAMP` is the transaction's start; `updated` is the change feed and must
// move forward on every write.
#[tokio::test(flavor = "multi_thread")]
async fn rows_written_in_one_transaction_carry_its_start_time() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    txn(&db, ann, "t-old", "acc-1", 1, "2026-03-05").await?;
    let written_alone: Timestamp = stored(&db, "transactions", "t-old").await?.get("updated")?;

    let tx = db.begin().await?;
    let began = tx.now().await?;
    assert!(began >= written_alone);
    txn(&tx, ann, "t-1", "acc-1", 1, "2026-03-05").await?;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    txn(&tx, ann, "t-2", "acc-1", 2, "2026-03-05").await?;
    update(&tx, "transactions", "t-old", data! {"label_memo": "edited"}, Scope::Owner(ann), vec![]).await?;
    // A hand-written insert leaves `updated` and `created_at` to their
    // defaults, which read the same clock.
    tx.insert(
        Insert::into("transaction_pairs")
            .set("pair_id", uuid(1))
            .set("user_id", ann)
            .set("transaction_id_a", "t-1")
            .set("transaction_id_b", "t-2")
            .set("status", "suggested"),
    )
    .await?;
    // The clock does not move inside the transaction, wherever it is read.
    assert_eq!(tx.now().await?, began);
    assert_eq!(tx.query_one(Query::table("users").key([ann]).select("at", now())).await?.get::<Timestamp>("at")?, began);
    tx.commit().await?;

    for id in ["t-old", "t-1", "t-2"] {
        assert_eq!(stored(&db, "transactions", id).await?.get::<Timestamp>("updated")?, began, "{id}");
    }
    let pair = stored(&db, "transaction_pairs", uuid(1)).await?;
    assert_eq!((pair.get::<Timestamp>("updated")?, pair.get::<Timestamp>("created_at")?), (began, began));
    // So a transaction that took a while commits rows stamped in the
    // past: the clock has moved on by at least the time it was open.
    let after = db.now().await?;
    assert!(after.micros() - began.micros() >= 30_000);
    // A statement on its own is stamped when it runs.
    update(&db, "transactions", "t-1", data! {"label_memo": "later"}, Scope::Owner(ann), vec![]).await?;
    let later: Timestamp = stored(&db, "transactions", "t-1").await?.get("updated")?;
    assert!(later >= after && later <= db.now().await?);
    Ok(())
}

/// Who owns a row the viewer may act on, so a new child row takes its
/// parent's owner and not the requester's. One function for every table, by
/// name and key.
async fn row_owner(db: &impl Db, table: &str, id: impl Into<Value>, viewer: Uuid) -> DbResult<Option<(Uuid, String)>> {
    let mut rows = Query::table(table).alias("r").key([id.into()]).filter(col("r.user_id").in_list(sharing_user_ids(viewer)));
    if soft_deletes(table) {
        rows = rows.filter(live_in("r"));
    }
    let owner = db
        .query_opt(
            rows.join(Query::table("users").alias("u").key([col("r.user_id")])).select("user_id", col("u.user_id")).select("username", col("u.username")),
        )
        .await?;
    owner.map(|row| Ok((row.get("user_id")?, row.get("username")?))).transpose()
}

// R-Q10, R-Q9, R-T7, R-X4: the sharing scope (in SQL a function `sharing_user_ids`, here a Rust function that
// returns an expression): membership is resolved per statement, and rows keep their owner.
#[tokio::test(flavor = "multi_thread")]
async fn the_sharing_scope_is_the_viewer_and_their_group_read_afresh_by_every_statement() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let (erin, frank) = (user(&db, "erin").await?, user(&db, "frank").await?);
    share(&db, uuid(0x6009), &[erin, frank]).await?;
    let sorted = |mut ids: Vec<Uuid>| {
        ids.sort();
        ids
    };

    // The function itself: the other members, then the viewer; for a
    // user in no group, the viewer alone. Groups do not leak into each
    // other.
    let as_list = |viewer: Uuid| {
        let db = &db;
        async move { db.query_one(Query::table("users").key([viewer]).select("ids", sharing_user_ids(viewer))).await?.get::<Vec<Uuid>>("ids") }
    };
    assert_eq!(as_list(home.alice).await?, [home.bob, home.alice]);
    assert_eq!(as_list(home.bob).await?, [home.alice, home.bob]);
    assert_eq!(as_list(home.carol).await?, [home.carol]);
    assert_eq!(sorted(as_list(erin).await?), sorted(vec![erin, frank]));
    // The same as rows, read after every mutation to fan events out.
    assert_eq!(sorted(sharing_group(&db, home.alice).await?), sorted(vec![home.alice, home.bob]));
    assert_eq!(sharing_group(&db, home.carol).await?, [home.carol]);

    for (owner, name) in [(home.alice, "alice"), (home.bob, "bob"), (home.carol, "carol")] {
        upsert(&db, "items", data! {"item_id": format!("item-{name}"), "user_id": owner, "provider": "bank"}, None, None).await?;
        insert(
            &db,
            "accounts",
            data! {"user_id": owner, "account_id": format!("acc-{name}"), "item_id": format!("item-{name}"), "institution_id": "ins-1"},
            &[],
            false,
        )
        .await?;
        txn(&db, owner, &format!("t-{name}"), &format!("acc-{name}"), 10, "2026-03-05").await?;
    }
    update(
        &db,
        "transactions",
        "t-bob",
        data! {"label_category_id": uuid(50), "label_budget_id": uuid(40), "label_category_confidence": 0.8},
        Scope::Owner(home.bob),
        vec![],
    )
    .await?;
    let viewer = Filters::new(Scope::Group(home.alice));

    // One row by key, if it is the group's.
    assert_eq!(find_one(&db, Query::table("items").key(["item-bob"]), &viewer).await?.expect("the group's item").get::<Uuid>("user_id")?, home.bob);
    assert!(find_one(&db, Query::table("items").key(["item-carol"]), &viewer).await?.is_none());
    assert!(find_one(&db, Query::table("transactions").key(["t-bob"]), &viewer).await?.is_some());
    assert!(find_one(&db, Query::table("transactions").key(["t-bob"]), &Filters::new(Scope::Group(home.carol))).await?.is_none());
    // Three columns of it, read before every edit of a transaction.
    let label = db
        .query_opt(
            find(Query::table("transactions").key(["t-bob"]), &viewer).columns(["label_category_id", "label_budget_id", "label_category_confidence"]).limit(1),
        )
        .await?
        .expect("the label");
    assert_eq!(label.columns(), ["label_category_id", "label_budget_id", "label_category_confidence"]);
    assert_eq!(
        (label.get::<Uuid>("label_category_id")?, label.get::<Uuid>("label_budget_id")?, label.get::<f64>("label_category_confidence")?),
        (uuid(50), uuid(40), 0.8)
    );

    // The owner of a parent row, joined to their name.
    assert_eq!(row_owner(&db, "accounts", "acc-bob", home.alice).await?, Some((home.bob, "bob".to_string())));
    assert_eq!(row_owner(&db, "items", "item-alice", home.bob).await?, Some((home.alice, "alice".to_string())));
    assert_eq!(row_owner(&db, "transactions", "t-alice", home.alice).await?, Some((home.alice, "alice".to_string())));
    assert_eq!(row_owner(&db, "accounts", "acc-carol", home.alice).await?, None);
    assert_eq!(row_owner(&db, "accounts", "acc-nowhere", home.alice).await?, None);
    soft_delete(&db, "accounts", "acc-bob", Scope::Group(home.alice)).await?;
    assert_eq!(row_owner(&db, "accounts", "acc-bob", home.alice).await?, None);
    // A webhook's item with its owner's name; no scope, since the provider
    // is the caller.
    let with_owner = db
        .query_one(
            Query::table("items")
                .alias("i")
                .key(["item-carol"])
                .filter(live_in("i"))
                .join(Query::table("users").alias("u").key([col("i.user_id")]))
                .columns(all_columns("items").iter().map(|c| format!("i.{c}")))
                .select("username", col("u.username")),
        )
        .await?;
    assert_eq!(with_owner.columns().len(), all_columns("items").len() + 1);
    assert_eq!(
        (with_owner.get::<String>("username")?, with_owner.get::<String>("provider")?, with_owner.get::<Uuid>("user_id")?),
        ("carol".to_string(), "bank".to_string(), home.carol)
    );

    // Bob leaves. The very next statement of either of them sees less; no
    // row changed owner on the way in or out.
    db.delete(Delete::rows(Query::table("sharing_members").keys([[home.alice], [home.bob]]))).await?;
    assert_eq!(as_list(home.alice).await?, [home.alice]);
    assert!(find_one(&db, Query::table("items").key(["item-bob"]), &viewer).await?.is_none());
    assert!(find_one(&db, Query::table("items").key(["item-alice"]), &Filters::new(Scope::Group(home.bob))).await?.is_none());
    assert_eq!(update_status(&update(&db, "transactions", "t-bob", data! {"label_memo": "not mine"}, Scope::Group(home.alice), vec![]).await?), 304);
    assert_eq!(stored(&db, "items", "item-bob").await?.get::<Uuid>("user_id")?, home.bob);
    // And joining again shows it all again.
    share(&db, home.group, &[home.alice, home.bob]).await?;
    assert_eq!(update_status(&update(&db, "transactions", "t-bob", data! {"label_memo": "ours"}, Scope::Group(home.alice), vec![]).await?), 200);
    Ok(())
}

/// The members of the viewer's group with their names. The viewer's group
/// is a scalar subquery; with no group it is NULL, the comparison is not
/// true for any row, and the list is empty.
fn group_members(viewer: Uuid) -> Query {
    let own_group = scalar(Query::table("sharing_members").alias("own").key([viewer]).select("group_id", col("own.group_id")));
    Query::table("sharing_members")
        .alias("m")
        .join(Query::table("users").alias("u").key([col("m.user_id")]))
        .filter(col("m.group_id").eq(own_group))
        .select("user_id", col("u.user_id"))
        .select("username", col("u.username"))
        .order_by(col("u.username"))
}

/// The invitations a viewer received, or that their group sent.
fn invitations_for(viewer: Uuid) -> Query {
    Query::table("sharing_invitations")
        .alias("i")
        .join(Query::table("users").alias("inviter").key([col("i.inviter_user_id")]))
        .join(Query::table("users").alias("invitee").key([col("i.invitee_user_id")]))
        .filter(col("i.invitee_user_id").eq(viewer).or(col("i.inviter_user_id").in_list(sharing_user_ids(viewer))))
        .select("invitation_id", col("i.invitation_id"))
        .select("inviter_user_id", col("inviter.user_id"))
        .select("inviter_username", col("inviter.username"))
        .select("invitee_user_id", col("invitee.user_id"))
        .select("invitee_username", col("invitee.username"))
        .order_by(col("invitee.username"))
        .order_by(col("i.invitation_id"))
}

/// The viewer's own address, folded the way invited addresses are stored.
fn own_email(viewer: Uuid) -> Expr {
    scalar(Query::table("users").alias("u").key([viewer]).select("email", col("u.email").lower()))
}

/// Which email invitations (`e`) concern a viewer: the ones their group
/// sent, and the unexpired ones addressed to them, by the server's clock.
fn concerns_by_email(viewer: Uuid) -> Expr {
    col("e.inviter_user_id").in_list(sharing_user_ids(viewer)).or(col("e.invitee_email").eq(own_email(viewer)).and(col("e.expires_at").gt(now())))
}

/// Those invitations, with whether the viewer's side sent them.
fn email_invitations_for(viewer: Uuid) -> Query {
    Query::table("sharing_email_invitations")
        .alias("e")
        .join(Query::table("users").alias("inviter").key([col("e.inviter_user_id")]))
        .filter(concerns_by_email(viewer))
        .select("invitation_id", col("e.invitation_id"))
        .select("invitee_email", col("e.invitee_email"))
        .select("expires_at", col("e.expires_at"))
        .select("inviter_user_id", col("inviter.user_id"))
        .select("inviter_username", col("inviter.username"))
        .select("sent", col("e.inviter_user_id").in_list(sharing_user_ids(viewer)))
        .order_by(col("e.invitee_email"))
        .order_by(col("e.invitation_id"))
}

/// The unique pair an email invitation is upserted on.
const EMAIL_PAIR: (&str, &[&str]) = ("sharing_email_invitations_pair_unique", &["inviter_user_id", "invitee_email"]);

async fn invite(db: &impl Db, from: Uuid, to: Uuid) -> DbResult<Uuid> {
    insert(db, "sharing_invitations", data! {"inviter_user_id": from, "invitee_user_id": to}, &["invitation_id"], true).await?.one()?.get("invitation_id")
}

async fn invite_email(db: &impl Db, from: Uuid, email: &str, expires_at: Timestamp) -> DbResult<Uuid> {
    upsert(db, "sharing_email_invitations", data! {"inviter_user_id": from, "invitee_email": email, "expires_at": expires_at}, None, Some(EMAIL_PAIR))
        .await?
        .one()?
        .get("invitation_id")
}

/// Alice, Bob and Zed share; Carol and Dave are on their own. Invitations
/// of both kinds cross between them.
struct Invitations {
    home: Household,
    zed: Uuid,
    dave: Uuid,
    carol_to_alice: Uuid,
    bob_to_dave: Uuid,
    dave_to_carol: Uuid,
    zed_to_carol: Uuid,
    alice_to_carol: Uuid,
    alice_to_carols_address: Uuid,
    bob_to_a_stranger: Uuid,
    dave_to_alices_address: Uuid,
    dave_to_carols_address_expired: Uuid,
}

async fn invitations(db: &impl Db) -> DbResult<Invitations> {
    let home = household(db).await?;
    let (zed, dave) = (user(db, "Zed").await?, user(db, "dave").await?);
    share(db, home.group, &[zed]).await?;
    update(db, "users", home.alice, data! {"email": "Alice@Example.com"}, Scope::Everyone, vec![]).await?;
    update(db, "users", home.carol, data! {"email": "Carol@Example.com"}, Scope::Everyone, vec![]).await?;
    let now = db.now().await?;
    let (next_week, a_minute_ago) = (now.plus_secs(7 * 86_400), now.plus_secs(-60));
    Ok(Invitations {
        carol_to_alice: invite(db, home.carol, home.alice).await?,
        bob_to_dave: invite(db, home.bob, dave).await?,
        dave_to_carol: invite(db, dave, home.carol).await?,
        zed_to_carol: invite(db, zed, home.carol).await?,
        alice_to_carol: invite(db, home.alice, home.carol).await?,
        alice_to_carols_address: invite_email(db, home.alice, "carol@example.com", next_week).await?,
        bob_to_a_stranger: invite_email(db, home.bob, "stranger@example.com", next_week).await?,
        dave_to_alices_address: invite_email(db, dave, "alice@example.com", next_week).await?,
        dave_to_carols_address_expired: invite_email(db, dave, "carol@example.com", a_minute_ago).await?,
        home,
        zed,
        dave,
    })
}

// R-Q9, R-Q10, R-Q8, R-T2, R-T9: what the sharing page reads, three statements at once: the members, the
// invitations and the invitations by email. The order of names reaches the reader.
#[tokio::test(flavor = "multi_thread")]
async fn members_and_invitations_are_listed_for_those_they_concern() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let all = invitations(&db).await?;
    let (alice, bob, carol) = (all.home.alice, all.home.bob, all.home.carol);
    let ids = |rows: &Rows| column::<Uuid>(rows, "invitation_id");

    let (members, direct, by_email) = tokio::join!(db.query(group_members(alice)), db.query(invitations_for(alice)), db.query(email_invitations_for(alice)));
    let (members, direct, by_email) = (members?, direct?, by_email?);

    // Names sort by their bytes, so a capital comes first.
    assert_eq!(texts_of(&members, "username"), ["Zed", "alice", "bob"]);
    assert_eq!(column::<Uuid>(&members, "user_id"), [all.zed, alice, bob]);
    assert!(db.query(group_members(carol)).await?.is_empty());

    // Alice was invited by Carol, and her group invited Carol (twice) and
    // Dave: by invitee's name, then by invitation id.
    let mut to_carol = [all.zed_to_carol, all.alice_to_carol];
    to_carol.sort();
    assert_eq!(ids(&direct), [all.carol_to_alice, to_carol[0], to_carol[1], all.bob_to_dave]);
    assert_eq!(texts_of(&direct, "invitee_username"), ["alice", "carol", "carol", "dave"]);
    assert_eq!(texts_of(&direct, "inviter_username")[0], "carol");
    assert_eq!((column::<Uuid>(&direct, "inviter_user_id")[3], column::<Uuid>(&direct, "invitee_user_id")[3]), (bob, all.dave));
    // Dave, in no group, sees what he sent and what he received.
    assert_eq!(ids(&db.query(invitations_for(all.dave)).await?), [all.dave_to_carol, all.bob_to_dave]);
    let mut carols = [all.dave_to_carol, all.zed_to_carol, all.alice_to_carol];
    carols.sort();
    assert_eq!(ids(&db.query(invitations_for(carol)).await?), [all.carol_to_alice, carols[0], carols[1], carols[2]]);

    // `sent` is computed per row: true for what the group sent, false for
    // what was addressed to the viewer.
    assert_eq!(ids(&by_email), [all.dave_to_alices_address, all.alice_to_carols_address, all.bob_to_a_stranger]);
    assert_eq!(texts_of(&by_email, "invitee_email"), ["alice@example.com", "carol@example.com", "stranger@example.com"]);
    assert_eq!(column::<bool>(&by_email, "sent"), [false, true, true]);
    assert_eq!(texts_of(&by_email, "inviter_username"), ["dave", "alice", "bob"]);
    let now = db.now().await?;
    assert!(column::<Timestamp>(&by_email, "expires_at").iter().all(|expiry| *expiry > now));
    // Bob has no address on file: he sees what the group sent, no more.
    assert_eq!(ids(&db.query(email_invitations_for(bob)).await?), [all.alice_to_carols_address, all.bob_to_a_stranger]);
    // Carol's address matches whatever its case; the expired invitation
    // to it is not shown to her, but still is to Dave who sent it.
    assert_eq!(ids(&db.query(email_invitations_for(carol)).await?), [all.alice_to_carols_address]);
    let daves = db.query(email_invitations_for(all.dave)).await?;
    assert_eq!(ids(&daves), [all.dave_to_alices_address, all.dave_to_carols_address_expired]);
    assert_eq!(column::<bool>(&daves, "sent"), [true, true]);
    Ok(())
}

// R-W7, R-W10, R-Q10, R-Q3: a conditional `DELETE .. RETURNING`, with a subquery in the predicate and in
// what is returned.
#[tokio::test(flavor = "multi_thread")]
async fn an_invitation_is_declined_or_withdrawn_only_by_those_it_concerns() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let all = invitations(&db).await?;
    let (alice, bob, carol) = (all.home.alice, all.home.bob, all.home.carol);

    // The invitee declines, the inviter's group withdraws. The returned row
    // is the permission check and the list of whom to tell.
    let answer = |viewer: Uuid, invitation: Uuid| {
        let concerned = col("invitee_user_id").eq(viewer).or(col("inviter_user_id").in_list(sharing_user_ids(viewer)));
        db.delete(Delete::from("sharing_invitations").key([invitation]).filter(concerned).returning(["inviter_user_id", "invitee_user_id"]))
    };
    let parties = |rows: Rows| rows.opt().map(|row| Ok::<_, DbError>((row.get::<Uuid>("inviter_user_id")?, row.get::<Uuid>("invitee_user_id")?))).transpose();
    assert_eq!(parties(answer(carol, all.bob_to_dave).await?)?, None);
    assert!(db.get("sharing_invitations", [all.bob_to_dave]).await?.is_some());
    assert_eq!(parties(answer(alice, all.carol_to_alice).await?)?, Some((carol, alice)));
    assert_eq!(parties(answer(bob, all.zed_to_carol).await?)?, Some((all.zed, carol)));
    assert_eq!(parties(answer(bob, all.zed_to_carol).await?)?, None);
    assert_eq!(parties(answer(all.dave, all.bob_to_dave).await?)?, Some((bob, all.dave)));
    assert_eq!(parties(answer(alice, uuid(404)).await?)?, None);
    assert_eq!(db.query(Query::table("sharing_invitations")).await?.len(), 2);

    // The same for an invitation by email. The invitee is whoever holds the
    // address now, looked up as the row goes.
    let answer_email = |viewer: Uuid, invitation: Uuid| {
        let holder =
            scalar(Query::table("users").alias("u").index("idx_users_lower_email", KeyRange::eq([col("e.invitee_email")])).select("user_id", col("u.user_id")));
        db.delete(
            Delete::rows(Query::table("sharing_email_invitations").alias("e").key([invitation]).filter(concerns_by_email(viewer)))
                .returning(["inviter_user_id"])
                .returning_expr("invitee_user_id", holder),
        )
    };
    let parties =
        |rows: Rows| rows.opt().map(|row| Ok::<_, DbError>((row.get::<Uuid>("inviter_user_id")?, row.get::<Option<Uuid>>("invitee_user_id")?))).transpose();
    // Expired, it can no longer be answered by the invitee, nor by a
    // stranger; it is still there.
    assert_eq!(parties(answer_email(carol, all.dave_to_carols_address_expired).await?)?, None);
    assert_eq!(parties(answer_email(alice, all.dave_to_carols_address_expired).await?)?, None);
    assert!(db.get("sharing_email_invitations", [all.dave_to_carols_address_expired]).await?.is_some());
    assert_eq!(parties(answer_email(carol, all.alice_to_carols_address).await?)?, Some((alice, Some(carol))));
    assert_eq!(parties(answer_email(carol, all.alice_to_carols_address).await?)?, None);
    // Nobody holds the stranger's address.
    assert_eq!(parties(answer_email(all.zed, all.bob_to_a_stranger).await?)?, Some((bob, None)));
    // The sender may withdraw an expired one.
    assert_eq!(parties(answer_email(all.dave, all.dave_to_carols_address_expired).await?)?, Some((all.dave, Some(carol))));
    assert_eq!(texts_of(&db.query(Query::table("sharing_email_invitations")).await?, "invitee_email"), ["alice@example.com"]);
    Ok(())
}

/// How many email invitations one user may have out.
const EMAIL_INVITATION_CAP: usize = 20;

/// Inviting by email: read the user's invitations, count them against the
/// cap, then send or renew. Postgres needs an advisory lock keyed on the
/// user so that two sends cannot both pass the count; a writing transaction
/// is alone from `begin` to `commit`.
async fn invite_by_email(db: &Client, inviter: Uuid, email: &str, expires_at: Timestamp) -> DbResult<Option<Row>> {
    let tx = db.begin().await?;
    let sent = tx.query(Query::table("sharing_email_invitations").index(EMAIL_PAIR.0, KeyRange::eq([inviter]))).await?;
    let renewal = texts_of(&sent, "invitee_email").iter().any(|address| address == email);
    if sent.len() >= EMAIL_INVITATION_CAP && !renewal {
        tx.rollback().await?;
        return Ok(None);
    }
    let row =
        upsert(&tx, "sharing_email_invitations", data! {"inviter_user_id": inviter, "invitee_email": email, "expires_at": expires_at}, None, Some(EMAIL_PAIR))
            .await?
            .one()?;
    tx.commit().await?;
    Ok(Some(row))
}

// R-X2, R-X3, R-W4, R-Q3: a read, a count and a write that must not interleave with another's; an upsert on a
// two-column unique key; a lookup through the index over `lower(email)`.
#[tokio::test(flavor = "multi_thread")]
async fn email_invitations_are_renewed_in_place_and_capped_even_when_sends_race() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let now = db.now().await?;
    let (soon, later) = (now.plus_secs(86_400), now.plus_secs(7 * 86_400));

    // A second send to the same address renews the first. The row, and its
    // id, stay; the pair is the conflict target and is not rewritten.
    let sent = invite_by_email(&db, home.alice, "dana@example.com", soon).await?.expect("under the cap");
    assert_eq!(sent.columns(), all_columns("sharing_email_invitations"));
    backdate(&db, "sharing_email_invitations", sent.get::<Uuid>("invitation_id")?, ts("2001-01-01T00:00:00Z")).await?;
    let renewed = invite_by_email(&db, home.alice, "dana@example.com", later).await?.expect("a renewal");
    assert_eq!(renewed.get::<Uuid>("invitation_id")?, sent.get::<Uuid>("invitation_id")?);
    assert_eq!(
        (renewed.get::<Timestamp>("expires_at")?, renewed.get::<Uuid>("inviter_user_id")?, renewed.get::<String>("invitee_email")?),
        (later, home.alice, "dana@example.com".to_string())
    );
    assert!(renewed.get::<Timestamp>("updated")? >= now);
    // Another sender's invitation to the same address is its own row.
    invite_by_email(&db, home.bob, "dana@example.com", soon).await?.expect("bob's own");
    assert_eq!(
        db.query(Query::table("sharing_email_invitations").index("idx_sharing_email_invitations_invitee_email", KeyRange::eq(["dana@example.com"])))
            .await?
            .len(),
        2
    );

    // The address is stored folded; whoever signs up with it in any case
    // is found through the index over `lower(email)`.
    let dana = insert(&db, "users", data! {"username": "dana", "email": "Dana@Example.COM"}, &[], false).await?.one()?.get::<Uuid>("user_id")?;
    let holder = db
        .query_one(
            Query::table("sharing_email_invitations")
                .alias("e")
                .key([sent.get::<Uuid>("invitation_id")?])
                .join(Query::table("users").alias("u").index("idx_users_lower_email", KeyRange::eq([col("e.invitee_email")])))
                .select("user_id", col("u.user_id")),
        )
        .await?;
    assert_eq!(holder.get::<Uuid>("user_id")?, dana);

    // The cap. With 18 out, six sends race for the two places left:
    // each reads the count and then writes, and no two of them can do
    // so at once.
    for i in 1..18 {
        invite_by_email(&db, home.alice, &format!("friend{i:02}@example.com"), soon).await?.expect("under the cap");
    }
    let racing: Vec<_> = (0..6)
        .map(|i| {
            let db = db.clone();
            tokio::spawn(async move { invite_by_email(&db, home.alice, &format!("late{i}@example.com"), soon).await })
        })
        .collect();
    let mut accepted = 0;
    for send in racing {
        if send.await.expect("the task ran")?.is_some() {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 2);
    let out = db.query(Query::table("sharing_email_invitations").index(EMAIL_PAIR.0, KeyRange::eq([home.alice]))).await?;
    assert_eq!(out.len(), EMAIL_INVITATION_CAP);
    // At the cap a new address is refused, a renewal is not, and Bob's
    // own count is his own.
    assert!(invite_by_email(&db, home.alice, "one-more@example.com", soon).await?.is_none());
    assert!(invite_by_email(&db, home.alice, "friend01@example.com", later).await?.is_some());
    assert!(invite_by_email(&db, home.bob, "one-more@example.com", soon).await?.is_some());
    assert_eq!(db.query(Query::table("sharing_email_invitations").index(EMAIL_PAIR.0, KeyRange::eq([home.alice]))).await?.len(), EMAIL_INVITATION_CAP);
    Ok(())
}

/// Accepting a sharing invitation. The invitation is of either kind (a
/// `UNION ALL` in SQL, two reads here); both users end in one group; every
/// invitation between members of that group is settled. Answers the group
/// as the transaction itself sees it, or `None` when the invitation is not
/// the viewer's to accept.
async fn accept_invitation(db: &Client, invitation: Uuid, viewer: Uuid, new_group: Uuid) -> DbResult<Option<Vec<Uuid>>> {
    let tx = db.begin().await?;
    let direct = Query::table("sharing_invitations").key([invitation]).filter(col("invitee_user_id").eq(viewer)).columns(["inviter_user_id"]);
    let by_email = Query::table("sharing_email_invitations")
        .alias("e")
        .key([invitation])
        .filter(col("e.inviter_user_id").ne(viewer))
        .filter(col("e.invitee_email").eq(own_email(viewer)).and(col("e.expires_at").gt(now())))
        .select("inviter_user_id", col("e.inviter_user_id"));
    let mut inviters: Vec<Uuid> = tx.query(direct).await?.column()?;
    inviters.extend(tx.query(by_email).await?.column::<Uuid>()?);
    let Some(inviter) = inviters.first().copied() else {
        tx.rollback().await?;
        return Ok(None);
    };

    // Where the two stand now. A user is in at most one group.
    let memberships = tx.query(Query::table("sharing_members").keys([[inviter], [viewer]])).await?;
    let groups: BTreeSet<Uuid> = column::<Uuid>(&memberships, "group_id").into_iter().collect();
    if groups.len() > 1 {
        tx.rollback().await?;
        return Ok(None);
    }
    let group = groups.first().copied().unwrap_or(new_group);
    // Both join it; one who is a member already is left alone.
    upsert_many(&tx, "sharing_members", vec![data! {"user_id": inviter, "group_id": group}, data! {"user_id": viewer, "group_id": group}], Some(&[])).await?;
    // `DELETE .. USING sharing_members inviter, sharing_members invitee`:
    // invitations whose two ends are now in the group.
    let member = |alias: &str, user: Expr| Query::table("sharing_members").alias(alias).key([user]).filter(col(&format!("{alias}.group_id")).eq(group));
    tx.delete(Delete::rows(
        Query::table("sharing_invitations").alias("i").join(member("inviter", col("i.inviter_user_id"))).join(member("invitee", col("i.invitee_user_id"))),
    ))
    .await?;
    // The same through the address: the invitee is the user whose folded
    // email is the invited one.
    tx.delete(Delete::rows(
        Query::table("sharing_email_invitations")
            .alias("e")
            .join(member("inviter", col("e.inviter_user_id")))
            .join(Query::table("users").alias("u").index("idx_users_lower_email", KeyRange::eq([col("e.invitee_email")])))
            .join(member("invitee", col("u.user_id"))),
    ))
    .await?;
    // The group as the transaction sees it, with the members just added.
    let members = sharing_group(&tx, viewer).await?;
    tx.commit().await?;
    Ok(Some(members))
}

// R-W8, R-W10, R-X1, R-X2, R-Q9, R-Q10: accepting an invitation in one transaction, with a `DELETE .. USING` two
// aliases of the membership table.
#[tokio::test(flavor = "multi_thread")]
async fn accepting_an_invitation_joins_the_group_and_settles_what_is_between_its_members() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let all = invitations(&db).await?;
    let (alice, bob, carol) = (all.home.alice, all.home.bob, all.home.carol);
    let sorted = |mut ids: Vec<Uuid>| {
        ids.sort();
        ids
    };
    account(&db, alice, "acc-alice").await?;
    assert!(find_one(&db, Query::table("accounts").key(["acc-alice"]), &Filters::new(Scope::Group(carol))).await?.is_none());

    // Not the viewer's to accept: someone else's invitation, one's own,
    // an expired one, and one that does not exist.
    assert_eq!(accept_invitation(&db, all.bob_to_dave, carol, uuid(1)).await?, None);
    assert_eq!(accept_invitation(&db, all.alice_to_carol, alice, uuid(1)).await?, None);
    assert_eq!(accept_invitation(&db, all.dave_to_carols_address_expired, carol, uuid(1)).await?, None);
    assert_eq!(accept_invitation(&db, uuid(404), carol, uuid(1)).await?, None);
    assert!(db.get("sharing_members", [carol]).await?.is_none());

    // Carol accepts Alice's invitation and joins the group Alice is in.
    let joined = accept_invitation(&db, all.alice_to_carol, carol, uuid(1)).await?.expect("hers to accept");
    assert_eq!(sorted(joined), sorted(vec![alice, bob, all.zed, carol]));
    assert_eq!(stored(&db, "sharing_members", carol).await?.get::<Uuid>("group_id")?, all.home.group);
    assert!(find_one(&db, Query::table("accounts").key(["acc-alice"]), &Filters::new(Scope::Group(carol))).await?.is_some());
    // Everything between two members is settled with it: the crossing
    // invitation and Zed's. What involves Dave, outside, stays.
    let left = db.query(Query::table("sharing_invitations")).await?;
    assert_eq!(sorted(column::<Uuid>(&left, "invitation_id")), sorted(vec![all.bob_to_dave, all.dave_to_carol]));
    let left = db.query(Query::table("sharing_email_invitations").order_by(col("invitee_email")).order_by(col("expires_at"))).await?;
    assert_eq!(column::<Uuid>(&left, "invitation_id"), [all.dave_to_alices_address, all.dave_to_carols_address_expired, all.bob_to_a_stranger]);

    // An invitation by email makes a new group when neither side has one.
    let erin = insert(&db, "users", data! {"username": "erin", "email": "ERIN@example.com"}, &[], false).await?.one()?.get::<Uuid>("user_id")?;
    let frank = user(&db, "frank").await?;
    let to_erin = invite_email(&db, frank, "erin@example.com", db.now().await?.plus_secs(3_600)).await?;
    assert_eq!(accept_invitation(&db, to_erin, frank, uuid(2)).await?, None);
    assert_eq!(sorted(accept_invitation(&db, to_erin, erin, uuid(2)).await?.expect("addressed to erin")), sorted(vec![erin, frank]));
    assert_eq!(stored(&db, "sharing_members", frank).await?.get::<Uuid>("group_id")?, uuid(2));
    assert!(db.get("sharing_email_invitations", [to_erin]).await?.is_none());
    // Two users who each have a group already cannot be merged this way.
    let across = invite(&db, erin, alice).await?;
    assert_eq!(accept_invitation(&db, across, alice, uuid(3)).await?, None);
    assert_eq!(sorted(sharing_group(&db, alice).await?), sorted(vec![alice, bob, all.zed, carol]));
    Ok(())
}

/// Removing a member from a sharing group: the group is read, the member
/// removed (a group of two is dissolved), and the transfer pairs that now
/// span the split are soft-deleted. Answers how many pairs that was.
async fn remove_member(db: &Client, viewer: Uuid, leaver: Uuid) -> DbResult<Option<u64>> {
    let tx = db.begin().await?;
    let former = sharing_group(&tx, viewer).await?;
    if !former.contains(&leaver) || former.len() < 2 {
        tx.rollback().await?;
        return Ok(None);
    }
    let removed = if former.len() == 2 { former.clone() } else { vec![leaver] };
    tx.delete(Delete::rows(Query::table("sharing_members").keys(removed.iter().map(|member| [*member])))).await?;
    // `UPDATE transaction_pairs pair .. FROM transactions a, transactions
    // b`: a pair is split when exactly one of its halves is the leaver's.
    let split = Query::table("transaction_pairs")
        .alias("pair")
        .index_ranges("idx_transaction_pairs_user_id_updated", former.iter().map(|member| KeyRange::eq([*member])))
        .join(Query::table("transactions").alias("a").key([col("pair.transaction_id_a")]))
        .join(Query::table("transactions").alias("b").key([col("pair.transaction_id_b")]))
        .filter(live_in("pair"))
        .filter(col("a.user_id").eq(leaver).ne(col("b.user_id").eq(leaver)));
    let pairs = tx.update(Update::rows(split).set("is_deleted", true).set("updated", now())).await?.affected();
    tx.commit().await?;
    Ok(Some(pairs))
}

// R-W8, R-W10, R-Q9, R-X1: removing a member in one transaction, with an `UPDATE .. FROM` two aliases of
// `transactions` and `(a.user_id = $2) <> (b.user_id = $2)`.
#[tokio::test(flavor = "multi_thread")]
async fn leaving_a_group_soft_deletes_the_pairs_that_span_the_split() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let zed = user(&db, "zed").await?;
    share(&db, home.group, &[zed]).await?;
    let long_ago = ts("2001-01-01T00:00:00Z");
    for (owner, name) in [(home.alice, "a"), (home.bob, "b"), (zed, "z"), (home.carol, "c")] {
        for n in 1..=3 {
            txn(&db, owner, &format!("t-{name}{n}"), &format!("acc-{name}"), 10, "2026-03-05").await?;
        }
    }
    let pairs = [
        (1, home.alice, "t-a1", "t-a2", false),
        (2, home.alice, "t-a3", "t-b1", false),
        (3, home.bob, "t-b2", "t-z1", false),
        (4, zed, "t-z2", "t-z3", false),
        (5, home.alice, "t-a1", "t-z2", true),
        (6, home.carol, "t-c1", "t-c2", false),
    ];
    for (n, owner, a, b, deleted) in pairs {
        db.insert(
            Insert::into("transaction_pairs")
                .set("pair_id", uuid(n))
                .set("user_id", owner)
                .set("transaction_id_a", a)
                .set("transaction_id_b", b)
                .set("status", "confirmed")
                .set("is_deleted", deleted)
                .set("updated", long_ago),
        )
        .await?;
    }
    let state =
        || async { Ok::<_, DbError>(column::<Option<bool>>(&db.query(Query::table("transaction_pairs").columns(["is_deleted"])).await?, "is_deleted")) };

    // Only a member can be removed, and only from one's own group.
    assert_eq!(remove_member(&db, home.carol, zed).await?, None);
    assert_eq!(remove_member(&db, home.alice, home.carol).await?, None);

    // Zed leaves. The one live pair with a half of Zed's and a half of
    // someone else's goes; Zed's own pair and the others' stay.
    assert_eq!(remove_member(&db, home.alice, zed).await?, Some(1));
    assert_eq!(state().await?, [Some(false), Some(false), Some(true), Some(false), Some(true), Some(false)]);
    assert!(stored(&db, "transaction_pairs", uuid(3)).await?.get::<Timestamp>("updated")? > long_ago);
    // The pair deleted before was not live and is not stamped again.
    assert_eq!(stored(&db, "transaction_pairs", uuid(5)).await?.get::<Timestamp>("updated")?, long_ago);
    assert_eq!(stored(&db, "transaction_pairs", uuid(4)).await?.get::<Timestamp>("updated")?, long_ago);
    assert_eq!(sharing_group(&db, zed).await?, [zed]);
    assert!(db.query(find(Query::table("transactions"), &Filters::new(Scope::Group(zed)))).await?.iter().all(|row| row.get::<Uuid>("user_id").unwrap() == zed));

    // Bob leaves the two that remain, which dissolves the group.
    assert_eq!(remove_member(&db, home.bob, home.bob).await?, Some(1));
    assert_eq!(state().await?, [Some(false), Some(true), Some(true), Some(false), Some(true), Some(false)]);
    assert!(db.query(Query::table("sharing_members")).await?.is_empty());
    assert_eq!(remove_member(&db, home.alice, home.bob).await?, None);
    Ok(())
}

// R-Q4, R-T3, R-Q2, R-Q6, R-T2: the change feed as a warm sync reads it: `updated >= cursor`, tombstones included,
// for transactions, pairs, recurrings and splits.
#[tokio::test(flavor = "multi_thread")]
async fn delta_sync_returns_what_changed_since_the_cursor_tombstones_included() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let start = db.now().await?;
    let an_hour_ago = start.plus_secs(-3600);
    for (id, owner) in [("t-a1", home.alice), ("t-a2", home.alice), ("t-a3", home.alice), ("t-b1", home.bob), ("t-b2", home.bob), ("t-c1", home.carol)] {
        txn(&db, owner, id, "acc-1", 10, "2026-03-05").await?;
        backdate(&db, "transactions", id, an_hour_ago).await?;
    }
    for (n, owner, a, b) in [(1, home.bob, "t-b1", "t-b2"), (2, home.alice, "t-a1", "t-b1"), (3, home.alice, "t-a2", "t-a3"), (4, home.carol, "t-c1", "t-a1")] {
        db.insert(
            Insert::into("transaction_pairs")
                .set("pair_id", uuid(n))
                .set("user_id", owner)
                .set("transaction_id_a", a)
                .set("transaction_id_b", b)
                .set("status", "suggested")
                .set("updated", an_hour_ago),
        )
        .await?;
    }
    for (n, owner, merchant) in [(1, home.alice, "netflix"), (2, home.bob, "gym"), (3, home.carol, "rent")] {
        db.insert(
            Insert::into("recurrings")
                .set("recurring_id", uuid(n))
                .set("user_id", owner)
                .set("merchant_key", merchant)
                .set("merchant_name", merchant)
                .set("cadence", "monthly")
                .set("is_income", false)
                .set("status", "confirmed")
                .set("updated", an_hour_ago),
        )
        .await?;
    }

    // The client's cursor is its own clock at the start of its last sync,
    // less a minute, sent as ISO text. `members` is the group, read once.
    let cursor = start.plus_secs(-60).to_rfc3339();
    let members = sharing_group(&db, home.alice).await?;
    let changed = |table: &str, index: &str, since: &str| {
        Query::table(table).index_ranges(index, members.iter().map(|member| KeyRange::within([*member], Bound::included([since]), Bound::Unbounded)))
    };
    let sorted = |mut ids: Vec<String>| {
        ids.sort();
        ids
    };
    // Nothing has changed since: the delta is empty, at the cost of one
    // short range per member.
    assert!(db.query(changed("transactions", "idx_transactions_user_id_updated", &cursor)).await?.is_empty());

    // An edit by a group member, a deletion, a new row; and an edit by
    // someone outside the group.
    update(&db, "transactions", "t-a2", data! {"label_memo": "edited"}, Scope::Group(home.bob), vec![]).await?;
    assert_eq!(bulk_soft_delete(&db, "transactions", vec!["t-b1".into()], Scope::Group(home.alice)).await?, 1);
    txn(&db, home.alice, "t-a4", "acc-1", 5, "2026-03-06").await?;
    update(&db, "transactions", "t-c1", data! {"label_memo": "carol's"}, Scope::Owner(home.carol), vec![]).await?;

    let delta = db.query(changed("transactions", "idx_transactions_user_id_updated", &cursor)).await?;
    assert_eq!(sorted(texts_of(&delta, "transaction_id")), ["t-a2", "t-a4", "t-b1"]);
    let tombstones: Vec<String> =
        delta.iter().filter(|row| row.get::<Option<bool>>("is_deleted").unwrap() == Some(true)).map(|row| row.get("transaction_id").unwrap()).collect();
    assert_eq!(tombstones, ["t-b1"]);
    // The builder's form of the same read, paged by key, agrees. Without
    // tombstones the deleted row is left out; an upper bound on `updated`
    // gives the rows that did not change.
    let since = Filters::new(Scope::Group(home.alice)).since("updated", cursor.as_str());
    let ids_of = |pages: Vec<Vec<Row>>| pages.concat().iter().map(|row| row.get("transaction_id")).collect::<DbResult<Vec<String>>>();
    assert_eq!(ids_of(find_in_batches(&db, "transactions", &since.clone().with_tombstones(), BATCH).await?)?, ["t-a2", "t-a4", "t-b1"]);
    assert_eq!(ids_of(find_in_batches(&db, "transactions", &since, BATCH).await?)?, ["t-a2", "t-a4"]);
    let before = Filters::new(Scope::Group(home.alice)).with_tombstones().since("updated", an_hour_ago).until("updated", cursor.as_str());
    assert_eq!(ids_of(find_in_batches(&db, "transactions", &before, BATCH).await?)?, ["t-a1", "t-a3", "t-b2"]);
    assert_eq!(
        ids_of(find_in_batches(&db, "transactions", &Filters::new(Scope::Group(home.carol)).since("updated", cursor.as_str()), BATCH).await?)?,
        ["t-c1"]
    );

    // The cursor is compared to the microsecond, and a row stamped
    // exactly at it is included.
    let exact = Timestamp(an_hour_ago.micros() - 3_600_000_000 + 2);
    backdate(&db, "transactions", "t-a1", exact).await?;
    let at = db.query(changed("transactions", "idx_transactions_user_id_updated", &exact.to_rfc3339())).await?;
    assert_eq!(sorted(texts_of(&at, "transaction_id")), ["t-a1", "t-a2", "t-a3", "t-a4", "t-b1", "t-b2"]);
    let after = db.query(changed("transactions", "idx_transactions_user_id_updated", &Timestamp(exact.micros() + 1).to_rfc3339())).await?;
    assert_eq!(sorted(texts_of(&after, "transaction_id")), ["t-a2", "t-a3", "t-a4", "t-b1", "t-b2"]);

    // The pairs feed, in key order. A rejected pair and a deleted one both
    // come back when tombstones are asked for: the client evicts them.
    // Uuids order by their bytes.
    assert_eq!(
        db.update(
            Update::table("transaction_pairs")
                .key([uuid(3)])
                .filter(col("user_id").in_list(sharing_user_ids(home.bob)))
                .filter(live())
                .set("status", "rejected")
                .set("updated", now())
        )
        .await?
        .affected(),
        1
    );
    assert_eq!(bulk_soft_delete_by_column(&db, "transaction_pairs", "transaction_id_a", vec!["t-b1"], Scope::Group(home.alice)).await?, 1);
    let pairs =
        db.query(find(Query::table("transaction_pairs"), &Filters::new(Scope::Group(home.alice)).with_tombstones().since("updated", cursor.as_str()))).await?;
    assert_eq!(column::<Uuid>(&pairs, "pair_id"), [uuid(1), uuid(3)]);
    assert_eq!(texts_of(&pairs, "status"), ["suggested", "rejected"]);
    assert_eq!(column::<Option<bool>>(&pairs, "is_deleted"), [Some(true), Some(false)]);
    let live_pairs = db.query(find(Query::table("transaction_pairs"), &Filters::new(Scope::Group(home.alice)))).await?;
    assert_eq!(column::<Uuid>(&live_pairs, "pair_id"), [uuid(2), uuid(3)]);
    let same_through_the_index = db.query(changed("transaction_pairs", "idx_transaction_pairs_user_id_updated", &cursor).order_by(col("pair_id"))).await?;
    assert_eq!(column::<Uuid>(&same_through_the_index, "pair_id"), [uuid(1), uuid(3)]);

    // Recurrings of the group, some ids of them, or a delta.
    update(&db, "recurrings", uuid(2), data! {"status": "dismissed"}, Scope::Owner(home.bob), vec![]).await?;
    let group = Filters::new(Scope::Group(home.alice));
    assert_eq!(column::<Uuid>(&db.query(find(Query::table("recurrings"), &group)).await?, "recurring_id"), [uuid(1), uuid(2)]);
    assert_eq!(
        column::<Uuid>(&db.query(find(Query::table("recurrings"), &group.clone().any_of("recurring_id", [uuid(2), uuid(3)]))).await?, "recurring_id"),
        [uuid(2)]
    );
    assert_eq!(column::<Uuid>(&db.query(find(Query::table("recurrings"), &group.clone().since("updated", cursor.as_str()))).await?, "recurring_id"), [uuid(2)]);

    // Splits of the group, newest first, by parent or as a delta.
    for (date, parent) in [("2026-03-01", "t-a2"), ("2026-03-09", "t-a2"), ("2026-03-05", "t-b2")] {
        insert(
            &db,
            "split_transactions",
            data! {"user_id": home.alice, "transaction_id": parent, "account_id": "acc-1", "amount": 1, "date": date},
            &[],
            false,
        )
        .await?;
    }
    let newest_first = |filters: &Filters| find(Query::table("split_transactions"), filters).order_by(col("date").desc());
    let dates = |rows: Rows| column::<Date>(&rows, "date").iter().map(Date::to_string).collect::<Vec<_>>();
    assert_eq!(
        dates(db.query(newest_first(&group.clone().with_tombstones().since("updated", cursor.as_str()))).await?),
        ["2026-03-09", "2026-03-05", "2026-03-01"]
    );
    assert_eq!(dates(db.query(newest_first(&group.clone().eq("transaction_id", "t-a2"))).await?), ["2026-03-09", "2026-03-01"]);
    assert!(db.query(newest_first(&Filters::new(Scope::Group(home.carol)))).await?.is_empty());
    Ok(())
}

// R-Q4, R-Q5, R-T3, R-T4, R-Q6: a date range is `col >= $n` and `col <= $m`, both inclusive, bound as text, on a
// `DATE` or a `TIMESTAMPTZ`.
#[tokio::test(flavor = "multi_thread")]
async fn date_ranges_are_inclusive_at_both_ends_on_dates_and_on_timestamps() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let days = [
        ("d-1", "acc-1", "2026-02-28"),
        ("d-2", "acc-1", "2026-03-01"),
        ("d-3", "acc-1", "2026-03-01"),
        ("d-4", "acc-1", "2026-03-15"),
        ("d-5", "acc-1", "2026-03-31"),
        ("d-6", "acc-1", "2026-04-01"),
        ("d-7", "acc-2", "2026-03-10"),
        ("d-8", "acc-3", "2026-03-11"),
    ];
    for (id, account, date) in days {
        let owner = if account == "acc-2" { home.bob } else { home.alice };
        txn(&db, owner, id, account, 10, date).await?;
        insert(
            &db,
            "investment_transactions",
            data! {"user_id": owner, "investment_transaction_id": id, "account_id": account, "date": date, "amount": 10},
            &[],
            false,
        )
        .await?;
    }
    txn(&db, home.carol, "d-9", "acc-1", 10, "2026-03-12").await?;
    soft_delete(&db, "transactions", "d-4", Scope::Owner(home.alice)).await?;
    let dates = |rows: &Rows| column::<Date>(rows, "date").iter().map(Date::to_string).collect::<Vec<_>>();
    let sorted = |mut ids: Vec<String>| {
        ids.sort();
        ids
    };

    // The stored window of some accounts, newest first, read through
    // `(account_id, date)`. The bounds are `YYYY-MM-DD` text and both days
    // belong to the window. The provider sync wants tombstones.
    let window = |table: &str, accounts: &[&str], from: Option<&str>, to: Option<&str>| {
        let bound = |day: Option<&str>| day.map_or(Bound::Unbounded, |day| Bound::included([day]));
        let index = format!("idx_{table}_account_id_date");
        Query::table(table)
            .index_ranges(&index, accounts.iter().map(|account| KeyRange::within([*account], bound(from), bound(to))))
            .filter(col("user_id").in_list(sharing_user_ids(home.alice)))
            .order_by(col("date").desc())
    };
    let march = db.query(window("transactions", &["acc-1", "acc-2"], Some("2026-03-01"), Some("2026-03-31"))).await?;
    assert_eq!(dates(&march), ["2026-03-31", "2026-03-15", "2026-03-10", "2026-03-01", "2026-03-01"]);
    assert_eq!(sorted(texts_of(&march, "transaction_id")), ["d-2", "d-3", "d-4", "d-5", "d-7"]);
    assert_eq!(dates(&db.query(window("transactions", &["acc-1"], Some("2026-03-31"), None)).await?), ["2026-04-01", "2026-03-31"]);
    assert_eq!(dates(&db.query(window("transactions", &["acc-1"], None, Some("2026-03-01"))).await?), ["2026-03-01", "2026-03-01", "2026-02-28"]);
    assert_eq!(dates(&db.query(window("transactions", &["acc-1"], Some("2026-03-02"), Some("2026-03-14"))).await?), Vec::<String>::new());
    assert_eq!(db.query(window("transactions", &["acc-1", "acc-2", "acc-3"], None, None)).await?.len(), 8);
    // A range of one day, and bounds given as dates instead of text.
    assert_eq!(db.query(window("transactions", &["acc-1"], Some("2026-03-01"), Some("2026-03-01"))).await?.len(), 2);
    let typed = Query::table("transactions").index(
        "idx_transactions_account_id_date",
        KeyRange::within(["acc-1"], Bound::included([Date::from_ymd(2026, 3, 15).unwrap()]), Bound::included([Date::from_ymd(2026, 3, 31).unwrap()])),
    );
    assert_eq!(dates(&db.query(typed).await?), ["2026-03-15", "2026-03-31"]);

    // The builder's own form filters instead of ranging, and leaves the
    // deleted row out unless asked.
    let filters = Filters::new(Scope::Group(home.alice)).any_of("account_id", ["acc-1", "acc-2"]).since("date", "2026-03-01").until("date", "2026-03-31");
    let built = db.query(find(Query::table("transactions"), &filters).order_by(col("date").desc())).await?;
    assert_eq!(dates(&built), ["2026-03-31", "2026-03-10", "2026-03-01", "2026-03-01"]);
    let built = db.query(find(Query::table("transactions"), &filters.clone().with_tombstones()).order_by(col("date").desc())).await?;
    assert_eq!(dates(&built), dates(&march));

    // The same statement on the other table, sent at the same time on a
    // second connection. It is also paged by offset, and can count what the
    // pages are cut from.
    let (transactions, investments) = tokio::join!(
        db.query(window("transactions", &["acc-1", "acc-2"], Some("2026-03-01"), Some("2026-03-31"))),
        db.query(window("investment_transactions", &["acc-1", "acc-2"], Some("2026-03-01"), Some("2026-03-31")))
    );
    assert_eq!(dates(&investments?), dates(&transactions?));
    let page = db.query(window("investment_transactions", &["acc-1"], Some("2026-03-01"), None).filter(live()).offset(1).limit(2).with_total()).await?;
    assert_eq!((dates(&page), page.total()), (vec!["2026-03-31".to_string(), "2026-03-15".to_string()], Some(5)));
    let last_page = db.query(window("investment_transactions", &["acc-1"], Some("2026-03-01"), None).offset(4).limit(2).with_total()).await?;
    assert_eq!((last_page.len(), last_page.total()), (1, Some(5)));

    // A security's price history, oldest first. `snapshot_date` is a
    // timestamp, and the bounds are instants to the microsecond.
    let instants = [
        "2026-02-28T23:59:59.999999Z",
        "2026-03-01T00:00:00Z",
        "2026-03-15T12:30:00Z",
        "2026-03-31T00:00:00Z",
        "2026-03-31T00:00:00.000001Z",
        "2026-04-02T00:00:00Z",
    ];
    for (n, at) in instants.iter().enumerate() {
        upsert(
            &db,
            "snapshots",
            data! {
                "snapshot_id": format!("sec-1-{n}"), "snapshot_date": *at, "snapshot_type": "security", "security_id": "sec-1",
                "close_price": 100 + n as i32,
            },
            None,
            None,
        )
        .await?;
        upsert(
            &db,
            "snapshots",
            data! {
                "snapshot_id": format!("hold-{n}"), "user_id": if n % 2 == 0 { home.alice } else { home.bob }, "snapshot_date": *at,
                "snapshot_type": "holding", "holding_account_id": "acc-1", "holding_security_id": "sec-1", "quantity": n as i32,
            },
            None,
            None,
        )
        .await?;
    }
    upsert(
        &db,
        "snapshots",
        data! {"snapshot_id": "sec-2-0", "snapshot_date": "2026-03-10T00:00:00Z", "snapshot_type": "security", "security_id": "sec-2", "close_price": 1},
        None,
        None,
    )
    .await?;
    soft_delete(&db, "snapshots", "sec-1-2", Scope::Everyone).await?;
    let history = |from: &str, to: &str| {
        Query::table("snapshots")
            .index("idx_snapshots_security_id_snapshot_date", KeyRange::within(["sec-1"], Bound::included([from]), Bound::included([to])))
            .filter(col("snapshot_type").eq("security"))
            .filter(live())
    };
    let prices = |rows: Rows| column::<Decimal>(&rows, "close_price");
    assert_eq!(prices(db.query(history("2026-03-01T00:00:00Z", "2026-03-31T00:00:00Z")).await?), [dec("101"), dec("103")]);
    assert_eq!(
        prices(db.query(history("2026-02-28T23:59:59.999999Z", "2026-03-31T00:00:00.000001Z")).await?),
        [dec("100"), dec("101"), dec("103"), dec("104")]
    );
    assert_eq!(prices(db.query(history("2026-03-01T00:00:00.000001Z", "2026-03-30T23:59:59.999999Z")).await?), Vec::<Decimal>::new());
    // A bare date as the bound is that day's first instant: the last day
    // of a range named this way contributes only its midnight.
    assert_eq!(prices(db.query(history("2026-03-01", "2026-03-31")).await?), [dec("101"), dec("103")]);
    assert_eq!(prices(db.query(history("2026-03-01", "2026-04-01")).await?), [dec("101"), dec("103"), dec("104")]);

    // The group's holding history in time order, through
    // `(user_id, snapshot_date, snapshot_id)`, one range per member.
    let members = sharing_group(&db, home.alice).await?;
    let holding_history = Query::table("snapshots")
        .index_ranges(
            "idx_snapshots_user_id_snapshot_date",
            members.iter().map(|member| KeyRange::within([*member], Bound::included(["2026-03-01T00:00:00Z"]), Bound::included(["2026-03-31T00:00:00Z"]))),
        )
        .filter(col("snapshot_type").eq("holding").and(col("holding_account_id").eq("acc-1")).and(col("holding_security_id").eq("sec-1")))
        .filter(live())
        .order_by(col("snapshot_date"));
    assert_eq!(texts_of(&db.query(holding_history).await?, "snapshot_id"), ["hold-1", "hold-2", "hold-3"]);
    // The builder's filter form of it, for a viewer outside the group.
    let none = find(
        Query::table("snapshots"),
        &Filters::new(Scope::Group(home.carol)).eq("snapshot_type", "holding").since("snapshot_date", "2026-03-01").until("snapshot_date", "2026-03-31"),
    );
    assert!(db.query(none.order_by(col("snapshot_date"))).await?.is_empty());
    Ok(())
}

/// The highest N among a user's rows named `Unknown_N`, deleted rows
/// included, or 0. Postgres would match `^Unknown_[0-9]+$` and capture the
/// digits; there are no regular expressions here, so the prefix is matched
/// with `like` (its underscore escaped) and the rest is read as an integer
/// where it is one.
fn highest_unknown(table: &str, index: &str, owner: Uuid) -> Query {
    let rest = col("name").substr(9, col("name").length());
    Query::table(table)
        .index(index, KeyRange::eq([owner]))
        .filter(col("name").like("Unknown\\_%"))
        .select("max", greatest([max(rest.try_cast(Type::Int)), val(0)]))
}

// R-Q7, R-Q11, R-Q15, R-Q16, R-Q22: what SQL says with a regex match, a capture, a cast, `MAX` and `COALESCE`.
#[tokio::test(flavor = "multi_thread")]
async fn the_next_unknown_name_comes_from_the_highest_suffix_in_use() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let unknown = |owner: Uuid| highest_unknown("transactions", "idx_transactions_user_id_transaction_id", owner);
    // With no such row at all the aggregate still answers, with 0.
    assert_eq!(db.query_one(unknown(home.alice)).await?.get::<i64>("max")?, 0);

    let names = [
        Some("Unknown_1"),
        Some("Unknown_12"),
        Some("Unknown_7"),
        Some("Unknown_x"),
        Some("Unknown_"),
        Some("Unknown_12b"),
        Some("unknown_99"),
        Some("UnknownX50"),
        Some("My Unknown_77"),
        Some("Rent"),
        None,
    ];
    for (n, name) in names.into_iter().enumerate() {
        insert(
            &db,
            "transactions",
            data! {"user_id": home.alice, "transaction_id": format!("t-{n:02}"), "account_id": "acc-1", "name": name, "date": "2026-03-05"},
            &[],
            false,
        )
        .await?;
    }
    insert(
        &db,
        "transactions",
        data! {"user_id": home.bob, "transaction_id": "t-bob", "account_id": "acc-2", "name": "Unknown_500", "date": "2026-03-05"},
        &[],
        false,
    )
    .await?;
    assert_eq!(db.query_one(unknown(home.alice)).await?.get::<i64>("max")?, 12);
    assert_eq!(db.query_one(unknown(home.bob)).await?.get::<i64>("max")?, 500);
    assert_eq!(db.query_one(unknown(home.carol)).await?.get::<i64>("max")?, 0);
    // A deleted row still holds its number.
    insert(
        &db,
        "transactions",
        data! {"user_id": home.alice, "transaction_id": "t-gone", "account_id": "acc-1", "name": "Unknown_40", "date": "2026-03-05"},
        &[],
        false,
    )
    .await?;
    soft_delete(&db, "transactions", "t-gone", Scope::Owner(home.alice)).await?;
    assert_eq!(db.query_one(unknown(home.alice)).await?.get::<i64>("max")?, 40);

    // The same statement on the other table.
    insert(
        &db,
        "investment_transactions",
        data! {"user_id": home.alice, "investment_transaction_id": "i-1", "account_id": "acc-1", "name": "Unknown_3", "date": "2026-03-05"},
        &[],
        false,
    )
    .await?;
    let invested = highest_unknown("investment_transactions", "idx_investment_transactions_user_id_investment_transaction_id", home.alice);
    assert_eq!(db.query_one(invested).await?.get::<i64>("max")?, 3);

    // The pieces: which names pass the prefix, and what the rest reads as.
    let parts = db
        .query(
            Query::table("transactions")
                .index("idx_transactions_user_id_transaction_id", KeyRange::eq([home.alice]))
                .filter(col("name").like("Unknown\\_%"))
                .select("name", col("name"))
                .select("n", col("name").substr(9, col("name").length()).try_cast(Type::Int)),
        )
        .await?;
    assert_eq!(texts_of(&parts, "name"), ["Unknown_1", "Unknown_12", "Unknown_7", "Unknown_x", "Unknown_", "Unknown_12b", "Unknown_40"]);
    assert_eq!(column::<Option<i64>>(&parts, "n"), [Some(1), Some(12), Some(7), None, None, None, Some(40)]);
    // `cast` fails loudly where `try_cast` gave NULL.
    let loud = Query::table("transactions").key(["t-03"]).select("n", col("name").substr(9, 10).cast(Type::Int));
    assert_eq!(code(db.query(loud).await), ErrorCode::TypeMismatch);
    Ok(())
}

/// Whether a pair names this transaction on either side and also passes
/// `also` (written for the pair as `tp`). An `OR` over two columns has no
/// single index, so each side is probed through its own.
fn pair_naming(transaction: Expr, also: Expr) -> Expr {
    let side = |index: &str| exists(Query::table("transaction_pairs").alias("tp").index(index, KeyRange::eq([transaction.clone()])).filter(also.clone()));
    side("idx_transaction_pairs_transaction_id_a").or(side("idx_transaction_pairs_transaction_id_b"))
}

/// Every settled, non-zero transaction of one user on a visible account
/// that is not half of a confirmed transfer, in id order. The join to
/// `accounts` has no owner or soft-delete condition, on purpose.
fn recurring_candidates(owner: Uuid) -> Query {
    let confirmed_transfer =
        pair_naming(col("t.transaction_id"), col("tp.user_id").eq(col("t.user_id")).and(live_in("tp")).and(col("tp.status").eq("confirmed")));
    Query::table("transactions")
        .alias("t")
        .index("idx_transactions_user_id_transaction_id", KeyRange::eq([owner]))
        .join(Query::table("accounts").alias("a").key([col("t.account_id")]))
        .filter(all([
            live_in("t"),
            coalesce([col("t.pending"), val(false)]).eq(false),
            col("t.amount").ne(0),
            coalesce([col("a.hide"), val(false)]).eq(false),
            confirmed_transfer.not(),
        ]))
        .select("transaction_id", col("t.transaction_id"))
        .select("amount", col("t.amount"))
        .select("date", col("t.date").cast(Type::Text))
        .select("merchant_name", col("t.merchant_name"))
        .select("name", col("t.name"))
}

// R-Q9, R-Q10, R-T9, R-Q22, R-Q6: an inner join, `COALESCE`, an anti-join with `OR`, `date::text`,
// `= ANY($2::text[])`.
#[tokio::test(flavor = "multi_thread")]
async fn recurring_candidates_skip_hidden_accounts_and_halves_of_confirmed_transfers() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    for (id, hide, deleted) in
        [("acc-seen", Some(false), false), ("acc-hidden", Some(true), false), ("acc-unset", None, false), ("acc-gone", Some(false), true)]
    {
        insert(
            &db,
            "accounts",
            data! {"user_id": home.alice, "account_id": id, "item_id": "item-1", "institution_id": "ins-1", "hide": hide, "is_deleted": deleted},
            &[],
            false,
        )
        .await?;
    }
    // (id, account, amount, pending)
    let rows: [(&str, &str, Value, Value); 15] = [
        ("r-01", "acc-seen", 15.99.into(), false.into()),
        ("r-02", "acc-seen", 15.99.into(), true.into()),
        ("r-03", "acc-seen", 9.5.into(), Value::Null),
        ("r-04", "acc-seen", 0.into(), false.into()),
        ("r-05", "acc-hidden", 20.into(), false.into()),
        ("r-06", "acc-unset", (-1200).into(), false.into()),
        ("r-07", "acc-seen", 30.into(), false.into()),
        ("r-08", "acc-seen", 500.into(), false.into()),
        ("r-09", "acc-seen", (-500).into(), false.into()),
        ("r-10", "acc-seen", 41.into(), false.into()),
        ("r-11", "acc-seen", 42.into(), false.into()),
        ("r-12", "acc-nowhere", 43.into(), false.into()),
        ("r-13", "acc-gone", 44.into(), false.into()),
        ("r-14", "acc-seen", Value::Null, false.into()),
        ("r-15", "acc-seen", 45.into(), false.into()),
    ];
    for (id, account, amount, pending) in rows {
        insert(
            &db,
            "transactions",
            data! {
                "user_id": home.alice, "transaction_id": id, "account_id": account, "amount": amount, "pending": pending, "date": "2026-03-05",
                "merchant_name": "Netflix", "name": "NETFLIX.COM",
            },
            &[],
            false,
        )
        .await?;
    }
    soft_delete(&db, "transactions", "r-07", Scope::Owner(home.alice)).await?;
    txn(&db, home.bob, "r-bob", "acc-seen", 15.99, "2026-03-05").await?;
    // (pair, owner, a, b, status, deleted)
    let pairs = [
        (1, home.alice, "r-08", "r-09", "confirmed", false),
        (2, home.alice, "r-10", "r-bob", "suggested", false),
        (3, home.alice, "r-11", "r-bob", "confirmed", true),
        (4, home.bob, "r-bob", "r-15", "confirmed", false),
    ];
    for (n, owner, a, b, status, deleted) in pairs {
        db.insert(
            Insert::into("transaction_pairs")
                .set("pair_id", uuid(n))
                .set("user_id", owner)
                .set("transaction_id_a", a)
                .set("transaction_id_b", b)
                .set("status", status)
                .set("is_deleted", deleted),
        )
        .await?;
    }

    // Out: pending, zero or no amount, a hidden or missing account, a
    // deleted row, both halves of the confirmed transfer. In: a pending
    // flag or a hide flag that is NULL, a deleted account, a pair that is
    // only suggested, deleted, or owned by someone else.
    let candidates = db.query(recurring_candidates(home.alice)).await?;
    assert_eq!(texts_of(&candidates, "transaction_id"), ["r-01", "r-03", "r-06", "r-10", "r-11", "r-13", "r-15"]);
    assert_eq!(candidates.columns(), ["transaction_id", "amount", "date", "merchant_name", "name"]);
    let first = candidates.first().unwrap();
    assert_eq!(
        (first.get::<Decimal>("amount")?, first.get::<String>("date")?, first.get::<String>("merchant_name")?),
        (dec("15.99"), "2026-03-05".to_string(), "Netflix".to_string())
    );
    assert_eq!(texts_of(&db.query(recurring_candidates(home.bob)).await?, "transaction_id"), Vec::<String>::new());
    // Once Bob's pair with r-15 is rejected, Bob's own half is a candidate.
    db.update(Update::table("transaction_pairs").key([uuid(4)]).set("status", "rejected").set("updated", now())).await?;
    assert_eq!(texts_of(&db.query(recurring_candidates(home.bob)).await?, "transaction_id"), ["r-bob"]);

    // The members of confirmed recurrings, by a list of ids; rows of other
    // users, deleted rows and unknown ids drop out.
    let members = Query::table("transactions")
        .alias("t")
        .keys([["r-13"], ["r-01"], ["r-07"], ["r-bob"], ["r-99"]])
        .filter(col("t.user_id").eq(home.alice).and(live_in("t")))
        .select("transaction_id", col("t.transaction_id"))
        .select("amount", col("t.amount"))
        .select("date", col("t.date").cast(Type::Text))
        .select("merchant_name", col("t.merchant_name"));
    assert_eq!(texts_of(&db.query(members).await?, "transaction_id"), ["r-13", "r-01"]);
    Ok(())
}

// R-Q18, R-T9: `pg_trgm.similarity`, on which the suggestion engine's results depend. There is no extension to
// create: the function is the server's own.
#[tokio::test(flavor = "multi_thread")]
async fn trigram_similarity_gives_the_numbers_pg_trgm_gives() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    // There is no query without a table: one row carries the expression.
    let similarity = |a: Value, b: Value| {
        let db = &db;
        async move { db.query_one(Query::table("users").key([ann]).select("s", call(Func::Similarity, [val(a), val(b)]))).await?.get::<Option<f64>>("s") }
    };

    // Each text is lower-cased and cut into words at anything that is
    // not a letter or digit; a word is padded with two blanks before and
    // one after; the measure is shared three-letter runs over all runs.
    // `word` has 5, `two words` 10, and they share 4: 4 / 11.
    let cases = [
        ("word", "two words", 4.0 / 11.0),
        ("Starbucks", "Starbucks", 1.0),
        ("Netflix", "NETFLIX", 1.0),
        ("abc", "xyz", 0.0),
        ("Starbucks", "Starbucks Coffee", 10.0 / 17.0),
        ("Netflix", "Netflix.com", 8.0 / 12.0),
        ("Amazon", "Amazon Prime", 7.0 / 13.0),
        ("Shell", "Shell Oil 12345", 6.0 / 16.0),
        ("hello", "hallo", 3.0 / 9.0),
        ("Starbucks", "STARBUCKS STORE 1234", 10.0 / 19.0),
        ("STARBUCKS STORE 1234", "STARBUCKS STORE 0042", 14.0 / 24.0),
        // The runs are a set: word order, punctuation and repeats do not count.
        ("two  words", "words,two", 1.0),
        ("aaaa", "aaa", 1.0),
        ("", "abc", 0.0),
    ];
    for (a, b, expected) in cases {
        let forwards = similarity(a.into(), b.into()).await?.expect("a number");
        let backwards = similarity(b.into(), a.into()).await?.expect("a number");
        // Postgres answers with a 4-byte float: 0.36363637 for 4 / 11.
        assert!((forwards - expected).abs() < 1e-6, "similarity({a:?}, {b:?}) = {forwards}, not {expected}");
        assert!((backwards - forwards).abs() < 1e-9, "similarity({b:?}, {a:?}) = {backwards}");
    }
    assert!((similarity("word".into(), "two words".into()).await?.unwrap() - 0.36363637).abs() < 1e-6);
    assert_eq!(similarity(Value::Null, "abc".into()).await?, None);
    assert_eq!(similarity("abc".into(), Value::Null).await?, None);

    // As the engine uses it: against a column, with its threshold of 0.5.
    for (id, merchant) in [("m-1", Some("Starbucks Coffee")), ("m-2", Some("STARBUCKS")), ("m-3", Some("Shell")), ("m-4", Some("Star Market")), ("m-5", None)] {
        insert(
            &db,
            "transactions",
            data! {"user_id": ann, "transaction_id": id, "account_id": "acc-1", "merchant_name": merchant, "date": "2026-03-05"},
            &[],
            false,
        )
        .await?;
    }
    let alike = call(Func::Similarity, [col("merchant_name"), val("Starbucks")]).ge(0.5);
    assert_eq!(matching(&db, ann, alike.clone()).await?, ["m-1", "m-2"]);
    // A row without a merchant is neither alike nor unlike.
    assert_eq!(matching(&db, ann, alike.not()).await?, ["m-3", "m-4"]);
    Ok(())
}

/// What the suggestion engine knows about the transaction it wants to
/// label: the parameters of its statement.
#[derive(Clone)]
struct Target {
    merchant_name: Option<&'static str>,
    name: Option<&'static str>,
    amount: f64,
    amount_band: (f64, f64),
    channel: Option<&'static str>,
    account_id: &'static str,
    provider_category: Option<&'static str>,
    /// Days of the month around the target's; may run below 1 or past 31.
    day_band: (i64, i64),
}

/// The score of a transaction `t`: how much it resembles the target, as
/// weighted points. SQL text would repeat it three times; here it is a
/// value, built once per use.
fn score(target: &Target) -> Expr {
    let points = |condition: Expr, points: i64| when(condition, points, 0);
    // `SIGN($13) = 0 OR SIGN(t.amount) = SIGN($13)`: money in is never
    // evidence for money out.
    let same_direction = || call(Func::Sign, [val(target.amount)]).eq(0).or(call(Func::Sign, [col("t.amount")]).eq(call(Func::Sign, [val(target.amount)])));
    let alike = |column: &str, text: Option<&str>| {
        val(text).is_not_null().and(col(column).is_not_null()).and(call(Func::Similarity, [col(column), val(text)]).ge(0.5)).and(same_direction())
    };
    let primary = col("t.raw").json_text(["category_hint", "primary"]);
    points(alike("t.merchant_name", target.merchant_name), 100)
        + points(alike("t.name", target.name), 50)
        + points(col("t.amount").between(target.amount_band.0, target.amount_band.1), 5)
        + points(val(target.channel).is_not_null().and(col("t.payment_channel").eq(target.channel)), 1)
        + points(col("t.account_id").eq(target.account_id), 1)
        + points(val(target.provider_category).is_not_null().and(primary.eq(target.provider_category)).and(same_direction()), 10)
        + points(call(Func::DatePart, [val("day"), col("t.date")]).between(target.day_band.0, target.day_band.1), 1)
}

/// What SQL would say with two CTEs, `scored` and `winning`: the owner's
/// confirmed, live history scored against the target, the rows above the
/// floor summed per category, and the category with the highest sum.
fn winning_category(owner: Uuid, target: &Target, floor: i64) -> Query {
    Query::table("transactions")
        .alias("t")
        .index("idx_transactions_user_id_transaction_id", KeyRange::eq([owner]))
        .filter(col("t.label_category_confidence").eq(1.0).and(live_in("t")).and(score(target).gt(floor)))
        .group_by([col("t.label_category_id")])
        .select("label_category_id", col("t.label_category_id"))
        .select("accepted", sum(score(target)))
        .select("count_matched", count())
        .order_by(sum(score(target)).desc())
        .limit(1)
}

/// The rest of the suggestion, for the winner: its budget, through two
/// joins that both carry the sharing scope, and how strongly the same
/// category was rejected before: the same score summed over the rejections
/// of it, joined to their transactions.
fn winner_details(owner: Uuid, category: Uuid, target: &Target, floor: i64) -> Query {
    let rejected = Query::table("rejected_categories")
        .alias("rc")
        .index("idx_rejected_categories_user_id", KeyRange::eq([owner]))
        .join(Query::table("transactions").alias("t").key([col("rc.transaction_id")]).filter(col("t.user_id").eq(col("rc.user_id"))))
        .filter(col("rc.category_id").eq(category).and(live_in("t")).and(score(target).gt(floor)))
        .select("rejected", coalesce([sum(score(target)), val(0)]));
    Query::table("categories")
        .alias("c")
        .key([category])
        .filter(col("c.user_id").in_list(sharing_user_ids(owner)))
        .join(Query::table("sections").alias("s").key([col("c.section_id")]).filter(col("s.user_id").in_list(sharing_user_ids(owner))))
        .select("label_budget_id", col("s.budget_id"))
        .select("rejected", scalar(rejected))
}

/// One run of the suggestion. Postgres would run it as one statement
/// over two CTEs; here it is two reads of one moment: the winner, then
/// what is joined to it.
/// Answers `(category, budget, accepted, count_matched, rejected)`.
async fn suggest(db: &Client, owner: Uuid, target: &Target, floor: i64) -> DbResult<Option<(Uuid, Uuid, i64, i64, i64)>> {
    let target = target.clone();
    db.read(|tx| async move {
        let Some(winner) = tx.query_opt(winning_category(owner, &target, floor)).await? else { return Ok(None) };
        let category: Uuid = winner.get("label_category_id")?;
        let Some(details) = tx.query_opt(winner_details(owner, category, &target, floor)).await? else { return Ok(None) };
        Ok(Some((category, details.get("label_budget_id")?, winner.get("accepted")?, winner.get("count_matched")?, details.get("rejected")?)))
    })
    .await
}

// R-Q11, R-Q13, R-Q16, R-Q18, R-Q9, R-Q10, R-Q21, R-T5, R-T9: a per-row weighted score, filtered, grouped and
// summed, top group first; then a correlated aggregate over a join.
#[tokio::test(flavor = "multi_thread")]
async fn auto_suggest_scores_the_confirmed_history_and_picks_the_top_category() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    // The budget is Bob's; Alice labels with it through the group. Fuel
    // is a category of Carol's, outside it.
    let budget: Uuid = insert(&db, "budgets", data! {"user_id": home.bob, "name": "Home"}, &[], false).await?.one()?.get("budget_id")?;
    let section: Uuid = insert(&db, "sections", data! {"user_id": home.bob, "budget_id": budget}, &[], false).await?.one()?.get("section_id")?;
    let carols_budget: Uuid = insert(&db, "budgets", data! {"user_id": home.carol}, &[], false).await?.one()?.get("budget_id")?;
    let carols_section: Uuid =
        insert(&db, "sections", data! {"user_id": home.carol, "budget_id": carols_budget}, &[], false).await?.one()?.get("section_id")?;
    let mut categories = std::collections::HashMap::new();
    for (name, owner, section) in [
        ("coffee", home.bob, section),
        ("treats", home.bob, section),
        ("refunds", home.bob, section),
        ("misc", home.bob, section),
        ("fuel", home.carol, carols_section),
    ] {
        let id: Uuid = insert(&db, "categories", data! {"user_id": owner, "section_id": section, "name": name}, &[], false).await?.one()?.get("category_id")?;
        categories.insert(name, id);
    }
    let hint = |primary: &str| Some(json!({"category_hint": {"primary": primary}}));

    // (id, owner, merchant, name, amount, channel, account, raw, date, category, confidence)
    type History = (
        &'static str,
        Uuid,
        Option<&'static str>,
        Option<&'static str>,
        f64,
        Option<&'static str>,
        &'static str,
        Option<Json>,
        &'static str,
        Option<&'static str>,
        f64,
    );
    let history: Vec<History> = vec![
        (
            "h1",
            home.alice,
            Some("Starbucks Coffee"),
            Some("STARBUCKS STORE 0042"),
            5.25,
            Some("in store"),
            "acc-1",
            hint("FOOD_AND_DRINK"),
            "2026-10-12",
            Some("coffee"),
            1.0,
        ),
        ("h2", home.alice, Some("Starbucks"), Some("Starbucks"), 20.0, Some("online"), "acc-2", hint("FOOD_AND_DRINK"), "2026-09-25", Some("coffee"), 1.0),
        ("h3", home.alice, Some("Shell"), Some("SHELL OIL"), 5.5, Some("in store"), "acc-1", hint("TRANSPORTATION"), "2026-10-14", Some("fuel"), 1.0),
        ("h4", home.alice, Some("Starbucks"), None, -5.75, Some("in store"), "acc-1", hint("FOOD_AND_DRINK"), "2026-08-13", Some("refunds"), 1.0),
        ("h5", home.alice, None, None, 6.0, None, "acc-1", None, "2026-10-11", Some("misc"), 1.0),
        (
            "h6",
            home.alice,
            Some("Starbucks Reserve"),
            Some("RESERVE ROASTERY"),
            30.0,
            Some("online"),
            "acc-3",
            hint("GENERAL_MERCHANDISE"),
            "2026-10-02",
            Some("treats"),
            1.0,
        ),
        // Not evidence: a mere suggestion, a deleted row, another user's.
        (
            "h7",
            home.alice,
            Some("Starbucks Coffee"),
            Some("STARBUCKS STORE 0042"),
            5.25,
            Some("in store"),
            "acc-1",
            hint("FOOD_AND_DRINK"),
            "2026-10-12",
            Some("treats"),
            0.7,
        ),
        (
            "h8",
            home.alice,
            Some("Starbucks Coffee"),
            Some("STARBUCKS STORE 0042"),
            5.25,
            Some("in store"),
            "acc-1",
            hint("FOOD_AND_DRINK"),
            "2026-10-12",
            Some("treats"),
            1.0,
        ),
        ("h9", home.bob, Some("Starbucks"), Some("Starbucks"), 5.25, Some("in store"), "acc-1", hint("FOOD_AND_DRINK"), "2026-10-12", Some("treats"), 1.0),
        // Rejections are scored on these; they carry no label themselves.
        ("r1", home.alice, Some("Starbucks"), None, 5.6, None, "acc-9", None, "2026-10-20", None, 0.0),
        ("r2", home.alice, Some("Starbucks"), None, 5.6, None, "acc-9", None, "2026-10-20", None, 0.0),
        ("r3", home.alice, Some("Shell"), None, 5.6, None, "acc-9", None, "2026-10-20", None, 0.0),
        ("r4", home.alice, Some("Starbucks"), None, 5.6, None, "acc-9", None, "2026-10-20", None, 0.0),
        ("r5", home.bob, Some("Starbucks"), None, 5.6, None, "acc-9", None, "2026-10-20", None, 0.0),
    ];
    for (id, owner, merchant, name, amount, channel, account, raw, date, category, confidence) in history {
        let row = data! {
            "user_id": owner, "transaction_id": id, "account_id": account, "merchant_name": merchant, "name": name, "amount": amount,
            "payment_channel": channel, "raw": raw, "date": date,
            "label_category_id": category.map(|name| categories[name]), "label_category_confidence": confidence,
        };
        insert(&db, "transactions", row, &[], false).await?;
    }
    soft_delete(&db, "transactions", "h8", Scope::Owner(home.alice)).await?;
    soft_delete(&db, "transactions", "r2", Scope::Owner(home.alice)).await?;
    for (transaction, owner, category) in
        [("r1", home.alice, "coffee"), ("r2", home.alice, "coffee"), ("r3", home.alice, "coffee"), ("r4", home.alice, "treats"), ("r5", home.bob, "coffee")]
    {
        db.insert(Insert::into("rejected_categories").set("transaction_id", transaction).set("user_id", owner).set("category_id", categories[category]))
            .await?;
    }

    // The unlabeled transaction: a 5.75 coffee on the 13th.
    let target = Target {
        merchant_name: Some("Starbucks"),
        name: Some("STARBUCKS STORE 1234"),
        amount: 5.75,
        amount_band: (5.0, 6.5),
        channel: Some("in store"),
        account_id: "acc-1",
        provider_category: Some("FOOD_AND_DRINK"),
        day_band: (10, 16),
    };

    // The score of each confirmed row, before any filter by it:
    //   h1 merchant 100, name 50, amount 5, channel 1, account 1, category 10, day 1
    //   h2 merchant 100, name 50, category 10
    //   h3 amount 5, channel 1, account 1, day 1 (another merchant)
    //   h4 channel 1, account 1, day 1 (the same merchant, but money in)
    //   h5 amount 5, account 1, day 1 (nothing to compare: all NULL)
    //   h6 merchant 100
    let scores = |target: &Target| {
        Query::table("transactions")
            .alias("t")
            .index("idx_transactions_user_id_transaction_id", KeyRange::eq([home.alice]))
            .filter(col("t.label_category_confidence").eq(1.0).and(live_in("t")))
            .select("id", col("t.transaction_id"))
            .select("score", score(target))
    };
    let scored: Vec<(String, i64)> = db.query(scores(&target)).await?.map()?;
    assert_eq!(scored, [("h1", 168), ("h2", 160), ("h3", 8), ("h4", 3), ("h5", 7), ("h6", 100)].map(|(id, score)| (id.to_string(), score)));

    // Above the floor of 19: coffee twice (328), treats once (100).
    assert_eq!(suggest(&db, home.alice, &target, 19).await?, Some((categories["coffee"], budget, 328, 2, 105)));
    // The second run, with the lower floor, changes nothing here.
    assert_eq!(suggest(&db, home.alice, &target, 15).await?, Some((categories["coffee"], budget, 328, 2, 105)));
    // A floor between the two coffee rows leaves one, and no rejection
    // scores that high; one above them all leaves no winner.
    assert_eq!(suggest(&db, home.alice, &target, 165).await?, Some((categories["coffee"], budget, 168, 1, 0)));
    assert_eq!(suggest(&db, home.alice, &target, 400).await?, None);
    // The groups behind the winner, top first.
    let ranking = db.query(winning_category(home.alice, &target, 19).limit(10)).await?;
    assert_eq!(column::<Uuid>(&ranking, "label_category_id"), [categories["coffee"], categories["treats"]]);
    assert_eq!((column::<i64>(&ranking, "accepted"), column::<i64>(&ranking, "count_matched")), (vec![328, 100], vec![2, 1]));

    // Each user's history is their own, and so are their rejections:
    // Bob's one confirmed row decides for Bob, his rejection of coffee
    // counted neither for Alice above nor against treats here. Carol has
    // no history at all.
    assert_eq!(suggest(&db, home.bob, &target, 19).await?, Some((categories["treats"], budget, 168, 1, 0)));
    assert_eq!(suggest(&db, home.carol, &target, 19).await?, None);

    // A target of amount zero has no direction, so the refund counts as
    // the same merchant; a target with nothing to compare scores only on
    // amount, account and day.
    let directionless = Target { amount: 0.0, ..target.clone() };
    let scored: Vec<(String, i64)> = db.query(scores(&directionless)).await?.map()?;
    assert_eq!(scored[3], ("h4".to_string(), 113));
    let bare = Target { merchant_name: None, name: None, channel: None, provider_category: None, ..target.clone() };
    let scored: Vec<(String, i64)> = db.query(scores(&bare)).await?.map()?;
    assert_eq!(scored.iter().map(|(_, score)| *score).collect::<Vec<_>>(), [7, 0, 7, 2, 7, 0]);

    // The winner must be a category of the group. Fuel wins for a tank
    // of petrol, but it is Carol's: the joins find nothing.
    let petrol = Target {
        merchant_name: Some("Shell"),
        name: None,
        amount: 40.0,
        amount_band: (30.0, 50.0),
        channel: None,
        account_id: "acc-x",
        provider_category: None,
        day_band: (-2, 40),
    };
    let winner = db.query_one(winning_category(home.alice, &petrol, 19)).await?;
    assert_eq!((winner.get::<Uuid>("label_category_id")?, winner.get::<i64>("accepted")?, winner.get::<i64>("count_matched")?), (categories["fuel"], 101, 1));
    assert_eq!(suggest(&db, home.alice, &petrol, 19).await?, None);
    Ok(())
}

// R-Q2, R-Q9, R-Q8, R-W7, R-X3: what feeds the suggestion engine, and its compare-and-swap label writes.
#[tokio::test(flavor = "multi_thread")]
async fn the_suggestion_engine_labels_a_row_only_while_it_is_still_unlabeled() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let today = day_of(db.now().await?);
    let days_ago = |n: i32| Date(today.0 - n);
    // (id, owner, date, confidence)
    let rows = [
        ("u-1", home.alice, days_ago(1), None),
        ("u-2", home.alice, days_ago(30), None),
        ("u-3", home.alice, days_ago(1), Some(1.0)),
        ("u-4", home.alice, days_ago(1), Some(0.0)),
        ("u-5", home.alice, days_ago(1), None),
        ("u-6", home.alice, days_ago(6), None),
        ("u-7", home.alice, days_ago(8), None),
        ("u-bob", home.bob, days_ago(1), None),
    ];
    for (id, owner, date, confidence) in rows {
        insert(
            &db,
            "transactions",
            data! {
                "user_id": owner, "transaction_id": id, "account_id": "acc-1", "merchant_name": "Corner Shop", "name": "CORNER SHOP", "amount": 12.5,
                "payment_channel": "in store", "date": date, "label_category_confidence": confidence, "raw": json!({"id": id}),
            },
            &[],
            false,
        )
        .await?;
    }
    soft_delete(&db, "transactions", "u-5", Scope::Owner(home.alice)).await?;

    // Every unlabeled live transaction of one user, whole rows. Zero is a
    // label state ("cleared"), not the absence of one.
    let unlabeled = db
        .query(find(
            Query::table("transactions").index("idx_transactions_user_id_transaction_id", KeyRange::eq([home.alice])),
            &Filters::new(Scope::Owner(home.alice)).eq("label_category_confidence", Value::Null),
        ))
        .await?;
    assert_eq!(texts_of(&unlabeled, "transaction_id"), ["u-1", "u-2", "u-6", "u-7"]);
    assert_eq!(unlabeled.first().unwrap().get::<Json>("raw")?, json!({"id": "u-1"}));

    // Unlabeled splits with their parent's features, for parents dated
    // within the last week by the server's clock.
    // (parent, split's owner, confidence, deleted)
    let splits = [
        ("u-1", home.alice, None, false),
        ("u-1", home.alice, Some(1.0), false),
        ("u-1", home.alice, None, true),
        ("u-2", home.alice, None, false),
        ("u-5", home.alice, None, false),
        ("u-6", home.alice, None, false),
        ("u-7", home.alice, None, false),
        ("u-bob", home.alice, None, false),
    ];
    let mut split_ids = Vec::new();
    for (parent, owner, confidence, deleted) in splits {
        let row = data! {
            "user_id": owner, "transaction_id": parent, "account_id": "acc-1", "amount": 2.5, "date": today, "label_category_confidence": confidence,
            "is_deleted": deleted,
        };
        split_ids.push(insert(&db, "split_transactions", row, &[], false).await?.one()?.get::<Uuid>("split_transaction_id")?);
    }
    let feed = Query::table("split_transactions")
        .alias("st")
        .index("idx_split_transactions_user_id_updated", KeyRange::eq([home.alice]))
        .join(Query::table("transactions").alias("t").key([col("st.transaction_id")]).filter(col("t.user_id").eq(col("st.user_id"))))
        .filter(all([col("st.label_category_confidence").is_null(), live_in("st"), live_in("t"), col("t.date").gt(now() - days(7))]))
        .select("split_transaction_id", col("st.split_transaction_id"))
        .select("merchant_name", col("t.merchant_name"))
        .select("name", col("t.name"))
        .select("amount", col("t.amount"))
        .select("payment_channel", col("t.payment_channel"))
        .select("account_id", col("t.account_id"))
        .select("raw", col("t.raw"))
        .select("date", col("t.date"));
    let fed = db.query(feed).await?;
    // The split on a labeled, deleted, month-old, eight-day-old or
    // foreign parent is not fed; nor is the labeled or the deleted split.
    let mut found = column::<Uuid>(&fed, "split_transaction_id");
    found.sort();
    let mut expected = vec![split_ids[0], split_ids[5]];
    expected.sort();
    assert_eq!(found, expected);
    let of_first = fed.iter().find(|row| row.get::<Uuid>("split_transaction_id").unwrap() == split_ids[0]).unwrap();
    // The features are the parent's, not the split's own.
    assert_eq!(
        (of_first.get::<String>("merchant_name")?, of_first.get::<Decimal>("amount")?, of_first.get::<Date>("date")?, of_first.get::<Json>("raw")?),
        ("Corner Shop".to_string(), dec("12.5"), days_ago(1), json!({"id": "u-1"}))
    );

    // The engine writes its label only if the row is still unlabeled when
    // the statement runs.
    let label = |id: &'static str, owner: Uuid, confidence: f64| {
        update(
            &db,
            "transactions",
            id,
            data! {"label_category_id": uuid(50), "label_budget_id": uuid(40), "label_category_confidence": confidence},
            Scope::Owner(owner),
            vec![col("label_category_confidence").is_null()],
        )
    };
    assert_eq!(update_status(&label("u-1", home.alice, 0.8).await?), 200);
    // A second engine, or the same one an hour later, changes nothing.
    assert_eq!(update_status(&label("u-1", home.alice, 0.6).await?), 304);
    assert_eq!(stored(&db, "transactions", "u-1").await?.get::<f64>("label_category_confidence")?, 0.8);
    // Nor does it touch what the user confirmed or cleared, a deleted
    // row, or another user's row.
    for id in ["u-3", "u-4", "u-5", "u-bob"] {
        assert_eq!(update_status(&label(id, home.alice, 0.9).await?), 304, "{id}");
    }
    assert_eq!(stored(&db, "transactions", "u-4").await?.get::<f64>("label_category_confidence")?, 0.0);
    assert!(stored(&db, "transactions", "u-bob").await?.is_null("label_category_id")?);
    // The user labels a row while the engine is deciding: the engine's
    // write, sent after, finds the row labeled.
    update(&db, "transactions", "u-2", data! {"label_category_id": uuid(51), "label_category_confidence": 1}, Scope::Group(home.bob), vec![]).await?;
    assert_eq!(update_status(&label("u-2", home.alice, 0.7).await?), 304);
    assert_eq!(stored(&db, "transactions", "u-2").await?.get::<Uuid>("label_category_id")?, uuid(51));
    // Two engines at once on one row: exactly one of them labels it.
    let (one, other) = tokio::join!(label("u-6", home.alice, 0.5), label("u-6", home.alice, 0.9));
    let outcomes = [update_status(&one?), update_status(&other?)];
    assert!(outcomes == [200, 304] || outcomes == [304, 200], "{outcomes:?}");

    // The same guard on a split.
    let label_split = |id: Uuid, confidence: f64| {
        update(
            &db,
            "split_transactions",
            id,
            data! {"label_category_id": uuid(50), "label_budget_id": uuid(40), "label_category_confidence": confidence},
            Scope::Owner(home.alice),
            vec![col("label_category_confidence").is_null()],
        )
    };
    assert_eq!(update_status(&label_split(split_ids[0], 0.8).await?), 200);
    assert_eq!(update_status(&label_split(split_ids[0], 0.6).await?), 304);
    assert_eq!(update_status(&label_split(split_ids[1], 0.6).await?), 304);
    assert_eq!(update_status(&label_split(split_ids[2], 0.6).await?), 304);
    assert_eq!(stored(&db, "split_transactions", split_ids[0]).await?.get::<f64>("label_category_confidence")?, 0.8);
    Ok(())
}

/// The first anti-join of the transfer statements: one of the group's live
/// pairs that is not rejected names `x` or `y`, on either side.
fn open_pair_touching(group: &[Uuid], x: Expr, y: Expr) -> Expr {
    let open = col("tp.user_id").in_list(uuid_list(group)).and(live_in("tp")).and(col("tp.status").ne("rejected"));
    pair_naming(x, open.clone()).or(pair_naming(y, open))
}

/// Their second anti-join: the group has a pair for exactly these two, in
/// either order, that also passes `also`. Read through the unique index.
fn pair_of(group: &[Uuid], x: Expr, y: Expr, also: Option<Expr>) -> Expr {
    let ordered = |a: Expr, b: Expr| {
        let mut rows = Query::table("transaction_pairs")
            .alias("tp")
            .index("transaction_pairs_pair_unique", KeyRange::eq([a, b]))
            .filter(col("tp.user_id").in_list(uuid_list(group)));
        if let Some(also) = &also {
            rows = rows.filter(also.clone());
        }
        exists(rows)
    };
    ordered(x.clone(), y.clone()).or(ordered(y, x))
}

/// Pairs of the group's live transactions that look like the two halves of
/// a transfer: exactly negated, non-zero amounts on different visible
/// accounts, within `window` days of each other, not already paired and not
/// rejected as a pair before. Closest dates first.
///
/// Where Postgres would hash-join on the amount, the second half is looked
/// up in `(user_id, amount, date)`: the negated amount is the key, the
/// window the range.
fn transfer_candidates(group: &[Uuid], window: i64, limit: u64) -> Query {
    let primary = |alias: &str| col(&format!("{alias}.raw")).json_text(["category_hint", "primary"]);
    let days_apart = call(Func::Abs, [col("t1.date") - col("t2.date")]);
    let other_half = Query::table("transactions")
        .alias("t2")
        .index_ranges(
            "idx_transactions_user_id_amount_date",
            group.iter().map(|member| {
                KeyRange::within([val(*member), -col("t1.amount")], Bound::included([col("t1.date") - window]), Bound::included([col("t1.date") + window]))
            }),
        )
        .filter(col("t1.amount").ne(0).and(col("t1.account_id").ne(col("t2.account_id"))).and(col("t1.transaction_id").lt(col("t2.transaction_id"))));
    let rejected_before = pair_of(group, col("t1.transaction_id"), col("t2.transaction_id"), Some(live_in("tp").and(col("tp.status").eq("rejected"))));
    Query::table("transactions")
        .alias("t1")
        .index_ranges("idx_transactions_user_id_date", group.iter().map(|member| KeyRange::eq([*member])))
        .join(other_half)
        .join(Query::table("accounts").alias("a1").key([col("t1.account_id")]).filter(col("a1.user_id").eq(col("t1.user_id"))))
        .join(Query::table("accounts").alias("a2").key([col("t2.account_id")]).filter(col("a2.user_id").eq(col("t2.user_id"))))
        .filter(all([
            live_in("t1"),
            live_in("t2"),
            coalesce([col("a1.hide"), val(false)]).eq(false),
            coalesce([col("a2.hide"), val(false)]).eq(false),
            open_pair_touching(group, col("t1.transaction_id"), col("t2.transaction_id")).not(),
            rejected_before.not(),
        ]))
        .select("transaction_id_a", col("t1.transaction_id"))
        .select("transaction_id_b", col("t2.transaction_id"))
        .select("user_id_a", col("t1.user_id"))
        .select("user_id_b", col("t2.user_id"))
        .select("date_delta", days_apart.clone())
        .select("is_bank_transfer", primary("t1").like("TRANSFER%").or(primary("t2").like("TRANSFER%")))
        .order_by(days_apart)
        .order_by(col("t1.transaction_id"))
        .limit(limit)
}

// R-T5, R-Q9, R-Q10, R-Q16, R-Q13, R-Q7, R-T9: the self-join on an exactly negated amount within a date window, two
// more joins, two anti-joins, computed columns, order by an expression, limit.
#[tokio::test(flavor = "multi_thread")]
async fn transfer_candidates_pair_exactly_negated_amounts_within_a_week() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    for (id, owner, hide) in [
        ("acc-chk", home.alice, false),
        ("acc-sav", home.alice, false),
        ("acc-hidden", home.alice, true),
        ("acc-bob", home.bob, false),
        ("acc-carol", home.carol, false),
    ] {
        insert(&db, "accounts", data! {"user_id": owner, "account_id": id, "item_id": "item-1", "institution_id": "ins-1", "hide": hide}, &[], false).await?;
    }
    let hint = |primary: &str| Some(json!({"category_hint": {"primary": primary}}));
    // (id, owner, account, amount, date, raw). The amounts arrive as a
    // caller binds them: floats, integers, text.
    type Booked = (&'static str, Uuid, &'static str, Value, &'static str, Option<Json>);
    let rows: Vec<Booked> = vec![
        ("t01", home.alice, "acc-chk", 500.into(), "2026-10-01", hint("TRANSFER_OUT")),
        ("t02", home.alice, "acc-sav", (-500).into(), "2026-10-03", hint("TRANSFER_IN")),
        ("t03", home.alice, "acc-chk", 120.5.into(), "2026-10-10", None),
        ("t04", home.bob, "acc-bob", "-120.50".into(), "2026-10-10", None),
        // Eight days apart; the same account; a cent off; zero.
        ("t05", home.alice, "acc-chk", 75.into(), "2026-10-01", None),
        ("t06", home.alice, "acc-sav", (-75).into(), "2026-10-09", None),
        ("t07", home.alice, "acc-chk", 75.into(), "2026-10-20", None),
        ("t08", home.alice, "acc-chk", (-75).into(), "2026-10-20", None),
        ("t09", home.alice, "acc-chk", 33.33.into(), "2026-10-05", None),
        ("t10", home.alice, "acc-sav", (-33.34).into(), "2026-10-05", None),
        ("t11", home.alice, "acc-chk", 0.into(), "2026-10-06", None),
        ("t12", home.alice, "acc-sav", 0.into(), "2026-10-06", None),
        // A hidden account; a deleted half; a half outside the group.
        ("t13", home.alice, "acc-chk", 60.into(), "2026-10-07", None),
        ("t14", home.alice, "acc-hidden", (-60).into(), "2026-10-07", None),
        ("t15", home.alice, "acc-chk", 41.into(), "2026-10-08", None),
        ("t16", home.alice, "acc-sav", (-41).into(), "2026-10-08", None),
        ("t17", home.alice, "acc-chk", 88.into(), "2026-10-12", None),
        ("t18", home.carol, "acc-carol", (-88).into(), "2026-10-12", None),
        // Already suggested as a pair, which also blocks t19 with t24.
        ("t19", home.alice, "acc-chk", 250.into(), "2026-10-14", None),
        ("t20", home.alice, "acc-sav", (-250).into(), "2026-10-15", None),
        ("t24", home.alice, "acc-sav", (-250).into(), "2026-10-14", None),
        // Rejected as a pair, which blocks only that pair: t21 with t23 is open.
        ("t21", home.alice, "acc-chk", 19.99.into(), "2026-10-16", hint("GENERAL_MERCHANDISE")),
        ("t22", home.alice, "acc-sav", (-19.99).into(), "2026-10-17", None),
        ("t23", home.alice, "acc-sav", (-19.99).into(), "2026-10-18", hint("FOOD_AND_DRINK")),
        // Thirty cents as floats do not add up to; stored, they match.
        ("t25", home.alice, "acc-chk", (0.1 + 0.2).into(), "2026-10-21", hint("FOOD_AND_DRINK")),
        ("t26", home.alice, "acc-sav", (-0.3).into(), "2026-10-21", hint("TRANSFER_IN")),
        // Exactly seven days apart.
        ("t27", home.alice, "acc-chk", 64.into(), "2026-10-01", hint("RENT_AND_UTILITIES")),
        ("t28", home.alice, "acc-sav", "-64.00".into(), "2026-10-08", None),
    ];
    for (id, owner, account, amount, date, raw) in rows {
        insert(
            &db,
            "transactions",
            data! {"user_id": owner, "transaction_id": id, "account_id": account, "amount": amount, "date": date, "raw": raw},
            &[],
            false,
        )
        .await?;
    }
    soft_delete(&db, "transactions", "t16", Scope::Owner(home.alice)).await?;
    for (n, a, b, status) in [(1, "t19", "t20", "suggested"), (2, "t21", "t22", "rejected")] {
        db.insert(
            Insert::into("transaction_pairs")
                .set("pair_id", uuid(n))
                .set("user_id", home.alice)
                .set("transaction_id_a", a)
                .set("transaction_id_b", b)
                .set("status", status),
        )
        .await?;
    }

    type Candidate = (String, String, Uuid, Uuid, i64, Option<bool>);
    let group = sharing_group(&db, home.alice).await?;
    let found: Vec<Candidate> = db.query(transfer_candidates(&group, 7, 500 * group.len() as u64)).await?.map()?;
    let candidate = |a: &str, b: &str, owner_b: Uuid, days: i64, transfer: Option<bool>| (a.to_string(), b.to_string(), home.alice, owner_b, days, transfer);
    // Same-day pairs first, then by the first half's id. The flag is
    // three-valued: unknown where neither payload says, or where one
    // says no and the other nothing.
    assert_eq!(
        found,
        [
            candidate("t03", "t04", home.bob, 0, None),
            candidate("t25", "t26", home.alice, 0, Some(true)),
            candidate("t01", "t02", home.alice, 2, Some(true)),
            candidate("t21", "t23", home.alice, 2, Some(false)),
            candidate("t27", "t28", home.alice, 7, None),
        ]
    );

    // The cap keeps the closest; a narrower window drops the far pair.
    let capped: Vec<Candidate> = db.query(transfer_candidates(&group, 7, 2)).await?.map()?;
    assert_eq!(capped, found[..2]);
    let narrow: Vec<Candidate> = db.query(transfer_candidates(&group, 6, 500)).await?.map()?;
    assert_eq!(narrow, found[..4]);
    // Carol's half has no other half inside her own group of one.
    assert!(db.query(transfer_candidates(&[home.carol], 7, 500)).await?.is_empty());
    // Bound to all three users, the same statement pairs it with Alice's.
    let everyone: Vec<Candidate> = db.query(transfer_candidates(&[home.alice, home.bob, home.carol], 7, 500)).await?.map()?;
    assert!(everyone.contains(&("t17".to_string(), "t18".to_string(), home.alice, home.carol, 0, None)));
    assert_eq!(everyone.len(), found.len() + 1);

    // Unhiding the account, restoring the deleted half and deleting the
    // suggestion each bring a pair in.
    update(&db, "accounts", "acc-hidden", data! {"hide": Value::Null}, Scope::Owner(home.alice), vec![]).await?;
    db.update(Update::table("transactions").key(["t16"]).set("is_deleted", false)).await?;
    soft_delete(&db, "transaction_pairs", uuid(1), Scope::Owner(home.alice)).await?;
    let more: Vec<Candidate> = db.query(transfer_candidates(&group, 7, 500)).await?.map()?;
    let added: Vec<(&str, &str)> = more.iter().filter(|c| !found.contains(c)).map(|c| (c.0.as_str(), c.1.as_str())).collect();
    assert_eq!(added, [("t13", "t14"), ("t15", "t16"), ("t19", "t24"), ("t19", "t20")]);
    Ok(())
}

/// Insert a suggestion unless either transaction is already in an open pair
/// of the group, or the group has a pair for these two at all. SQL's
/// `INSERT .. SELECT .. WHERE NOT EXISTS` selects constants from no table;
/// here the row of the first transaction, which the foreign key requires to
/// exist, carries them.
fn suggest_pair(group: &[Uuid], pair_id: Uuid, owner: Uuid, a: &str, b: &str) -> Insert {
    let blocked = open_pair_touching(group, val(a), val(b)).or(pair_of(group, val(a), val(b), None));
    let proposed = Query::table("transactions")
        .key([a])
        .filter(blocked.not())
        .select("pair_id", val(pair_id))
        .select("user_id", val(owner))
        .select("transaction_id_a", val(a))
        .select("transaction_id_b", val(b))
        .select("status", val("suggested"));
    Insert::into("transaction_pairs").columns(["pair_id", "user_id", "transaction_id_a", "transaction_id_b", "status"]).from_query(proposed)
}

/// Soft-delete the group's live pairs of which a half has been deleted.
fn sweep_stale_pairs(group: &[Uuid]) -> Update {
    let a_deleted_half =
        exists(Query::table("transactions").alias("t").keys([[col("tp.transaction_id_a")], [col("tp.transaction_id_b")]]).filter(col("t.is_deleted").eq(true)));
    let pairs = Query::table("transaction_pairs")
        .alias("tp")
        .index_ranges("idx_transaction_pairs_user_id_updated", group.iter().map(|member| KeyRange::eq([*member])))
        .filter(live_in("tp").and(a_deleted_half));
    Update::rows(pairs).set("is_deleted", true).set("updated", now())
}

/// What one run of the transfer detection did.
#[derive(Debug, PartialEq)]
struct Detection {
    swept: u64,
    /// What the job reports as suggested: every insert that did not fail,
    /// whether or not its anti-joins let a row in.
    suggested: usize,
    inserted: u64,
    failed: usize,
}

/// The hourly transfer detection for one sharing group: one transaction
/// that reads the group, sweeps stale pairs, finds the candidates and
/// inserts each under a savepoint, so that an insert that fails loses only
/// itself. Postgres would take two advisory locks first; the writing
/// transaction is their equal.
async fn detect_transfers(db: &Client, viewer: Uuid, run: u128) -> DbResult<Detection> {
    let tx = db.begin().await?;
    let group = sharing_group(&tx, viewer).await?;
    let mut done = Detection { swept: tx.update(sweep_stale_pairs(&group)).await?.affected(), suggested: 0, inserted: 0, failed: 0 };
    let candidates = tx.query(transfer_candidates(&group, 7, 500 * group.len() as u64)).await?;
    for (n, candidate) in candidates.iter().enumerate() {
        let (a, b, owner): (String, String, Uuid) = (candidate.get("transaction_id_a")?, candidate.get("transaction_id_b")?, candidate.get("user_id_a")?);
        tx.savepoint("pair_attempt").await?;
        match tx.insert(suggest_pair(&group, uuid(run * 1000 + n as u128), owner, &a, &b)).await {
            Ok(rows) => {
                tx.release("pair_attempt").await?;
                done.suggested += 1;
                done.inserted += rows.affected();
            }
            // Logged, and on to the next.
            Err(_) => {
                tx.rollback_to("pair_attempt").await?;
                done.failed += 1;
            }
        }
    }
    tx.commit().await?;
    Ok(done)
}

// R-X6, R-X1, R-X2, R-W3, R-W11, R-Q10: a job that inserts each of its rows under a savepoint, so the one unique
// violation it expects as an error costs that row alone.
#[tokio::test(flavor = "multi_thread")]
async fn transfer_detection_keeps_its_other_pairs_when_one_insert_hits_a_unique_violation() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    for (id, owner) in [("acc-chk", home.alice), ("acc-sav", home.alice), ("acc-bob", home.bob)] {
        account(&db, owner, id).await?;
    }
    let rows = [
        ("x1", home.alice, "acc-chk", 10, "2026-10-01"),
        ("x2", home.alice, "acc-sav", -10, "2026-10-01"),
        ("x3", home.alice, "acc-chk", 20, "2026-10-01"),
        ("x4", home.bob, "acc-bob", -20, "2026-10-01"),
        ("x5", home.bob, "acc-bob", 30, "2026-10-01"),
        ("x6", home.alice, "acc-sav", -30, "2026-10-01"),
        ("x7", home.alice, "acc-chk", 77, "2026-09-01"),
        ("x8", home.alice, "acc-sav", 78, "2026-09-01"),
        // One money-out with two possible money-ins, a day apart.
        ("y1", home.alice, "acc-chk", 40, "2026-10-02"),
        ("y2", home.alice, "acc-sav", -40, "2026-10-02"),
        ("y3", home.bob, "acc-bob", -40, "2026-10-03"),
    ];
    for (id, owner, account, amount, date) in rows {
        txn(&db, owner, id, account, amount, date).await?;
    }
    // A pair one of whose halves was deleted since: stale.
    db.insert(
        Insert::into("transaction_pairs")
            .set("pair_id", uuid(1))
            .set("user_id", home.alice)
            .set("transaction_id_a", "x7")
            .set("transaction_id_b", "x8")
            .set("status", "confirmed"),
    )
    .await?;
    soft_delete(&db, "transactions", "x8", Scope::Owner(home.alice)).await?;
    // A pair for x3 and x4 that belongs to Carol, who left the group: the
    // anti-joins, bound to the group, cannot see it. The unique key can.
    db.insert(
        Insert::into("transaction_pairs")
            .set("pair_id", uuid(2))
            .set("user_id", home.carol)
            .set("transaction_id_a", "x3")
            .set("transaction_id_b", "x4")
            .set("status", "suggested"),
    )
    .await?;
    let group = [home.alice, home.bob];

    // The statement on its own, outside any transaction: the violation
    // is an error that names the constraint, and writes nothing.
    let refused = db.insert(suggest_pair(&group, uuid(3), home.alice, "x3", "x4")).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::UniqueViolation);
    assert!(refused.violates("transaction_pairs_pair_unique"));

    // The run: candidates come closest first, then by id, so the failing
    // insert is the second of five. The pairs before and after it are
    // committed all the same.
    let first = detect_transfers(&db, home.alice, 1).await?;
    assert_eq!(first, Detection { swept: 1, suggested: 4, inserted: 3, failed: 1 });
    let pairs =
        db.query(Query::table("transaction_pairs").filter(col("user_id").in_list(uuid_list(&group))).filter(live()).order_by(col("transaction_id_a"))).await?;
    assert_eq!(texts_of(&pairs, "transaction_id_a"), ["x1", "x5", "y1"]);
    assert_eq!(texts_of(&pairs, "transaction_id_b"), ["x2", "x6", "y2"]);
    assert!(texts_of(&pairs, "status").iter().all(|status| status == "suggested"));
    // A pair is owned by the owner of its first half, not by whoever ran.
    assert_eq!(column::<Uuid>(&pairs, "user_id"), [home.alice, home.bob, home.alice]);
    // y1 with y3 was a candidate when the run began; by its turn y1 was
    // paired, in this same transaction, and the anti-join made the insert
    // a no-op. The job still counted it.
    assert!(db.query(Query::table("transaction_pairs").index("idx_transaction_pairs_transaction_id_b", KeyRange::eq(["y3"]))).await?.is_empty());
    assert_eq!(stored(&db, "transaction_pairs", uuid(1)).await?.get::<Option<bool>>("is_deleted")?, Some(true));
    let carols = stored(&db, "transaction_pairs", uuid(2)).await?;
    assert_eq!(
        (carols.get::<Uuid>("user_id")?, carols.get::<String>("status")?, carols.get::<Option<bool>>("is_deleted")?),
        (home.carol, "suggested".to_string(), Some(false))
    );

    // An hour later nothing is stale and everything open is paired; only
    // the pair that cannot be inserted is tried, and fails, again.
    assert_eq!(detect_transfers(&db, home.bob, 2).await?, Detection { swept: 0, suggested: 0, inserted: 0, failed: 1 });
    assert_eq!(db.query(Query::table("transaction_pairs")).await?.len(), 5);

    // Without the savepoint the same failure would have cost the run:
    // the transaction is aborted, and its commit is refused.
    let tx = db.begin().await?;
    tx.insert(suggest_pair(&group, uuid(4), home.alice, "y1", "y3")).await?;
    assert_eq!(code(tx.insert(suggest_pair(&group, uuid(5), home.alice, "x3", "x4")).await), ErrorCode::UniqueViolation);
    assert_eq!(code(tx.query(Query::table("transaction_pairs")).await), ErrorCode::TransactionAborted);
    assert_eq!(code(tx.commit().await), ErrorCode::TransactionAborted);
    assert_eq!(db.query(Query::table("transaction_pairs")).await?.len(), 5);

    // A savepoint is also undone by name more than once, and released
    // work stays: the second attempt's row is kept, the first and third
    // are not.
    let tx = db.begin().await?;
    for (n, (a, b)) in [("x3", "x4"), ("x1", "x4"), ("x3", "x4")].into_iter().enumerate() {
        tx.savepoint("pair_attempt").await?;
        match tx
            .insert(
                Insert::into("transaction_pairs")
                    .set("pair_id", uuid(10 + n as u128))
                    .set("user_id", home.alice)
                    .set("transaction_id_a", a)
                    .set("transaction_id_b", b)
                    .set("status", "suggested"),
            )
            .await
        {
            Ok(_) => tx.release("pair_attempt").await?,
            Err(_) => tx.rollback_to("pair_attempt").await?,
        }
    }
    tx.commit().await?;
    assert_eq!(db.query(Query::table("transaction_pairs")).await?.len(), 6);
    assert!(db.get("transaction_pairs", [uuid(11)]).await?.is_some());
    Ok(())
}

/// Store a pair on its unique `(transaction_id_a, transaction_id_b)`. A new
/// pair is inserted. A deleted or rejected one is revived with the offered
/// status and a fresh `updated`. A live suggested or confirmed one keeps
/// its status and its `updated`, so no delta is sent. Returns the id of the
/// row that now stands.
fn upsert_pair(pair_id: Uuid, owner: Uuid, a: &str, b: &str, status: &str) -> Insert {
    let deleted = || col("is_deleted").eq(true);
    let rejected = || col("status").eq("rejected");
    Insert::into("transaction_pairs")
        .set("pair_id", pair_id)
        .set("user_id", owner)
        .set("transaction_id_a", a)
        .set("transaction_id_b", b)
        .set("status", status)
        .on_conflict_update("transaction_pairs_pair_unique")
        .update("status", case([(deleted(), excluded("status")), (rejected(), excluded("status"))], col("status")))
        .update("is_deleted", false)
        .update("updated", when(deleted().or(rejected()), now(), col("updated")))
        .returning(["pair_id"])
}

// R-W4, R-S3, R-W11, R-S6: a conflict on a two-column unique key, `CASE` over the existing row, tombstone revival,
// `RETURNING` the surviving row. Tombstones stay in unique keys.
#[tokio::test(flavor = "multi_thread")]
async fn a_pair_upsert_inserts_revives_a_tombstone_or_leaves_a_live_pair_alone() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    for id in ["a", "b", "c", "d", "e", "f"] {
        txn(&db, ann, id, "acc-1", 1, "2026-03-05").await?;
    }
    let long_ago = ts("2001-01-01T00:00:00Z");
    let state = |pair: Uuid| {
        let db = &db;
        async move {
            let row = stored(db, "transaction_pairs", pair).await?;
            Ok::<_, DbError>((row.get::<String>("status")?, row.get::<Option<bool>>("is_deleted")?, row.get::<Timestamp>("updated")? > long_ago))
        }
    };

    // New: the offered id is the one that stands; the defaults fill the rest.
    let created = db.insert(upsert_pair(uuid(1), ann, "a", "b", "suggested")).await?;
    assert_eq!((created.affected(), created.one()?.get::<Uuid>("pair_id")?), (1, uuid(1)));
    let row = stored(&db, "transaction_pairs", uuid(1)).await?;
    assert_eq!((row.get::<Option<bool>>("is_deleted")?, row.get::<Timestamp>("created_at")?), (Some(false), row.get::<Timestamp>("updated")?));

    // Live and suggested: the status offered is ignored, `updated` is
    // not touched, and the id that comes back is the older one.
    backdate(&db, "transaction_pairs", uuid(1), long_ago).await?;
    let kept = db.insert(upsert_pair(uuid(2), ann, "a", "b", "confirmed")).await?.one()?;
    assert_eq!(kept.get::<Uuid>("pair_id")?, uuid(1));
    assert_eq!(state(uuid(1)).await?, ("suggested".to_string(), Some(false), false));
    assert!(db.get("transaction_pairs", [uuid(2)]).await?.is_none());

    // Rejected: revived with the offered status and stamped.
    db.update(Update::table("transaction_pairs").key([uuid(1)]).set("status", "rejected")).await?;
    assert_eq!(db.insert(upsert_pair(uuid(3), ann, "a", "b", "confirmed")).await?.one()?.get::<Uuid>("pair_id")?, uuid(1));
    assert_eq!(state(uuid(1)).await?, ("confirmed".to_string(), Some(false), true));

    // Soft-deleted, whatever its status was: revived the same way.
    soft_delete(&db, "transaction_pairs", uuid(1), Scope::Owner(ann)).await?;
    backdate(&db, "transaction_pairs", uuid(1), long_ago).await?;
    assert_eq!(db.insert(upsert_pair(uuid(4), ann, "a", "b", "suggested")).await?.one()?.get::<Uuid>("pair_id")?, uuid(1));
    assert_eq!(state(uuid(1)).await?, ("suggested".to_string(), Some(false), true));
    // A flag that is NULL counts as live.
    db.update(Update::table("transaction_pairs").key([uuid(1)]).set("is_deleted", Value::Null).set("updated", long_ago)).await?;
    db.insert(upsert_pair(uuid(5), ann, "a", "b", "confirmed")).await?;
    assert_eq!(state(uuid(1)).await?, ("suggested".to_string(), Some(false), false));
    assert_eq!(db.query(Query::table("transaction_pairs")).await?.len(), 1);

    // The key is the ordered pair: the application stores the two ids in
    // its own canonical order, and the same two the other way round are
    // another row to the database.
    assert_eq!(db.insert(upsert_pair(uuid(6), ann, "b", "a", "suggested")).await?.one()?.get::<Uuid>("pair_id")?, uuid(6));
    // Only that key is the conflict target: an id already taken by
    // another pair is an error, as is a pair that names no transaction.
    let same_id = db.insert(upsert_pair(uuid(1), ann, "c", "d", "suggested")).await.unwrap_err();
    assert_eq!(same_id.code, ErrorCode::UniqueViolation);
    assert!(same_id.violates(PRIMARY));
    assert_eq!(code(db.insert(upsert_pair(uuid(7), ann, "c", "nowhere", "suggested")).await), ErrorCode::ForeignKeyViolation);
    assert_eq!(code(db.insert(upsert_pair(uuid(7), uuid(404), "c", "d", "suggested")).await), ErrorCode::ForeignKeyViolation);
    assert_eq!(db.query(Query::table("transaction_pairs")).await?.len(), 2);
    Ok(())
}

/// The group's live pairs that name either transaction, on either side.
fn pairs_touching(viewer: Uuid, a: &str, b: &str) -> Query {
    Query::table("transaction_pairs").filter(col("user_id").in_list(sharing_user_ids(viewer))).filter(live()).filter(any([
        col("transaction_id_a").eq(a),
        col("transaction_id_b").eq(a),
        col("transaction_id_a").eq(b),
        col("transaction_id_b").eq(b),
    ]))
}

/// Every other suggestion that names either half is rejected.
fn reject_rivals(viewer: Uuid, pair: Uuid, a: &str, b: &str) -> Update {
    Update::rows(pairs_touching(viewer, a, b).filter(col("status").eq("suggested")).filter(col("pair_id").ne(pair)))
        .set("status", "rejected")
        .set("updated", now())
}

/// Why a pairing or a confirmation did nothing. These are answers, not
/// errors: the transaction is rolled back and the caller says so.
#[derive(Debug, PartialEq)]
enum Refused {
    NotFound,
    Collision,
}

/// The user says two transactions are one transfer. Postgres would read
/// them `FOR SHARE` so a concurrent delete could not slip in before the
/// pair was written; here nothing else writes until the commit.
async fn pair_transactions(db: &Client, viewer: Uuid, x: &str, y: &str, offered: Uuid) -> DbResult<Result<Uuid, Refused>> {
    let (a, b) = if x < y { (x, y) } else { (y, x) };
    let tx = db.begin().await?;
    // Both must be live rows of the group; the caller compares how many
    // came back with how many it asked for.
    let halves = tx
        .query(
            Query::table("transactions")
                .keys([[a], [b]])
                .filter(col("user_id").in_list(sharing_user_ids(viewer)))
                .filter(live())
                .columns(["transaction_id", "user_id"]),
        )
        .await?;
    if halves.len() != 2 {
        tx.rollback().await?;
        return Ok(Err(Refused::NotFound));
    }
    let owner: Uuid = halves.first().expect("two rows").get("user_id")?;
    // Is either half in another confirmed pair?
    let other_pair = col("transaction_id_a").eq(a).and(col("transaction_id_b").eq(b)).not();
    if tx.query_opt(pairs_touching(viewer, a, b).filter(col("status").eq("confirmed")).filter(other_pair).columns(["pair_id"]).limit(1)).await?.is_some() {
        tx.rollback().await?;
        return Ok(Err(Refused::Collision));
    }
    let pair: Uuid = tx.insert(upsert_pair(offered, owner, a, b, "confirmed")).await?.one()?.get("pair_id")?;
    tx.update(reject_rivals(viewer, pair, a, b)).await?;
    tx.commit().await?;
    Ok(Ok(pair))
}

/// A suggestion becomes a confirmed pair, unless it is gone or one of its
/// halves has been confirmed elsewhere.
async fn confirm_pair(db: &Client, viewer: Uuid, pair: Uuid) -> DbResult<Result<(), Refused>> {
    let group = || col("user_id").in_list(sharing_user_ids(viewer));
    let tx = db.begin().await?;
    // The group's pair, live and not rejected (`FOR UPDATE` in SQL).
    let Some(row) = tx
        .query_opt(
            Query::table("transaction_pairs")
                .key([pair])
                .filter(group())
                .filter(live())
                .filter(col("status").ne("rejected"))
                .columns(["transaction_id_a", "transaction_id_b"]),
        )
        .await?
    else {
        tx.rollback().await?;
        return Ok(Err(Refused::NotFound));
    };
    let (a, b): (String, String) = (row.get("transaction_id_a")?, row.get("transaction_id_b")?);
    // Is either half in another confirmed pair?
    if tx
        .query_opt(pairs_touching(viewer, &a, &b).filter(col("status").eq("confirmed")).filter(col("pair_id").ne(pair)).columns(["pair_id"]).limit(1))
        .await?
        .is_some()
    {
        tx.rollback().await?;
        return Ok(Err(Refused::Collision));
    }
    // Confirm it, then reject its rivals.
    tx.update(Update::table("transaction_pairs").key([pair]).filter(group()).filter(live()).set("status", "confirmed").set("updated", now())).await?;
    tx.update(reject_rivals(viewer, pair, &a, &b)).await?;
    tx.commit().await?;
    Ok(Ok(()))
}

// R-X1, R-X2, R-W4, R-W6, R-Q6: pairing in one transaction, where SQL reads `FOR SHARE` against a concurrent soft
// delete; a new row takes its parent's owner.
#[tokio::test(flavor = "multi_thread")]
async fn pairing_two_transactions_checks_both_exist_and_rejects_rival_suggestions() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    for (id, owner) in [
        ("p1", home.alice),
        ("p2", home.alice),
        ("p3", home.bob),
        ("p4", home.alice),
        ("p5", home.carol),
        ("p6", home.alice),
        ("p7", home.bob),
        ("p8", home.alice),
    ] {
        txn(&db, owner, id, "acc-1", 1, "2026-03-05").await?;
    }
    soft_delete(&db, "transactions", "p6", Scope::Owner(home.alice)).await?;
    let long_ago = ts("2001-01-01T00:00:00Z");
    for (n, a, b) in [(1, "p1", "p3"), (2, "p2", "p4"), (3, "p3", "p4")] {
        db.insert(
            Insert::into("transaction_pairs")
                .set("pair_id", uuid(n))
                .set("user_id", home.alice)
                .set("transaction_id_a", a)
                .set("transaction_id_b", b)
                .set("status", "suggested")
                .set("updated", long_ago),
        )
        .await?;
    }
    let statuses =
        || async { Ok::<_, DbError>(texts_of(&db.query(Query::table("transaction_pairs").keys([[uuid(1)], [uuid(2)], [uuid(3)]])).await?, "status")) };

    // A half that is deleted, outside the group, or unknown: not found,
    // and nothing is written.
    for missing in ["p6", "p5", "p-none"] {
        assert_eq!(pair_transactions(&db, home.alice, "p1", missing, uuid(9)).await?, Err(Refused::NotFound), "{missing}");
    }
    assert_eq!(db.query(Query::table("transaction_pairs")).await?.len(), 3);

    // Alice pairs p2 with p1, naming them the other way round. The pair is
    // stored in canonical order; the suggestions that named either half are
    // rejected and stamped; the one that named neither is not.
    let paired = pair_transactions(&db, home.alice, "p2", "p1", uuid(10)).await?.expect("paired");
    assert_eq!(paired, uuid(10));
    let row = stored(&db, "transaction_pairs", uuid(10)).await?;
    assert_eq!(
        (row.get::<String>("transaction_id_a")?, row.get::<String>("transaction_id_b")?, row.get::<String>("status")?),
        ("p1".to_string(), "p2".to_string(), "confirmed".to_string())
    );
    assert_eq!(statuses().await?, ["rejected", "rejected", "suggested"]);
    assert!(stored(&db, "transaction_pairs", uuid(1)).await?.get::<Timestamp>("updated")? > long_ago);
    assert_eq!(stored(&db, "transaction_pairs", uuid(3)).await?.get::<Timestamp>("updated")?, long_ago);

    // A half that is confirmed elsewhere cannot be paired again.
    assert_eq!(pair_transactions(&db, home.bob, "p1", "p3", uuid(11)).await?, Err(Refused::Collision));
    assert_eq!(statuses().await?[0], "rejected");
    // Pairing the same two again is not a collision with itself: the
    // pair that stands answers, under its first id.
    assert_eq!(pair_transactions(&db, home.bob, "p1", "p2", uuid(12)).await?, Ok(uuid(10)));
    // A lone rejection whose result nobody reads; then pairing them once
    // more revives the very same row.
    db.update(
        Update::table("transaction_pairs")
            .key([uuid(10)])
            .filter(col("user_id").in_list(sharing_user_ids(home.bob)))
            .filter(live())
            .set("status", "rejected")
            .set("updated", now()),
    )
    .await?;
    assert_eq!(pair_transactions(&db, home.alice, "p1", "p2", uuid(13)).await?, Ok(uuid(10)));
    assert_eq!(stored(&db, "transaction_pairs", uuid(10)).await?.get::<String>("status")?, "confirmed");

    // The pair belongs to whoever owns its first half, here Bob, though
    // Alice asked.
    let across = pair_transactions(&db, home.alice, "p8", "p7", uuid(14)).await?.expect("paired");
    assert_eq!(stored(&db, "transaction_pairs", across).await?.get::<Uuid>("user_id")?, home.bob);
    // Carol sees none of it, and cannot pair what is not hers.
    assert_eq!(pair_transactions(&db, home.carol, "p3", "p4", uuid(15)).await?, Err(Refused::NotFound));

    // A delete that commits first is seen by the pairing that follows:
    // where Postgres makes the `FOR SHARE` read wait and re-check, the
    // pairing simply begins after it.
    assert_eq!(bulk_soft_delete(&db, "transactions", vec!["p4".into()], Scope::Group(home.alice)).await?, 1);
    assert_eq!(pair_transactions(&db, home.alice, "p3", "p4", uuid(16)).await?, Err(Refused::NotFound));
    assert_eq!(statuses().await?[2], "suggested");
    Ok(())
}

// R-X2, R-X3, R-W7, R-X1: read, check, write under a lock: of several racing to do a once-only thing, one succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn racing_confirmations_of_rival_pairs_let_exactly_one_through() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    for (id, owner) in [
        ("c0", home.alice),
        ("c1", home.alice),
        ("c2", home.bob),
        ("c3", home.alice),
        ("c4", home.bob),
        ("c5", home.alice),
        ("c6", home.bob),
        ("d1", home.alice),
        ("d2", home.alice),
        ("d3", home.alice),
        ("e1", home.carol),
        ("e2", home.carol),
    ] {
        txn(&db, owner, id, "acc-1", 1, "2026-03-05").await?;
    }
    let suggest = |n: u128, owner: Uuid, a: &'static str, b: &'static str| {
        db.insert(
            Insert::into("transaction_pairs")
                .set("pair_id", uuid(n))
                .set("user_id", owner)
                .set("transaction_id_a", a)
                .set("transaction_id_b", b)
                .set("status", "suggested"),
        )
    };

    // What cannot be confirmed: a pair that does not exist, is someone
    // else's, was rejected, or was deleted.
    suggest(20, home.carol, "e1", "e2").await?;
    suggest(21, home.alice, "d1", "d2").await?;
    suggest(22, home.alice, "d1", "d3").await?;
    db.update(Update::table("transaction_pairs").key([uuid(21)]).set("status", "rejected")).await?;
    soft_delete(&db, "transaction_pairs", uuid(22), Scope::Owner(home.alice)).await?;
    for pair in [uuid(404), uuid(20), uuid(21), uuid(22)] {
        assert_eq!(confirm_pair(&db, home.alice, pair).await?, Err(Refused::NotFound));
    }
    assert_eq!(confirm_pair(&db, home.carol, uuid(20)).await?, Ok(()));
    // A suggestion made after one of its halves was confirmed elsewhere
    // is refused by the collision probe, and stays a suggestion.
    suggest(23, home.alice, "d2", "d3").await?;
    assert_eq!(confirm_pair(&db, home.bob, uuid(23)).await?, Ok(()));
    suggest(24, home.alice, "d1", "d2").await.unwrap_err();
    suggest(25, home.alice, "d3", "d1").await?;
    assert_eq!(confirm_pair(&db, home.alice, uuid(25)).await?, Err(Refused::Collision));
    assert_eq!(stored(&db, "transaction_pairs", uuid(25)).await?.get::<String>("status")?, "suggested");

    // Six suggestions all claim c0. Six requests confirm one each, at
    // once. Each reads its pair, probes for a collision and writes; were
    // two to interleave, both would find no collision and both confirm.
    for (n, other) in ["c1", "c2", "c3", "c4", "c5", "c6"].into_iter().enumerate() {
        suggest(n as u128 + 1, home.alice, "c0", other).await?;
    }
    let racing: Vec<_> = (1..=6)
        .map(|n| {
            let db = db.clone();
            let viewer = if n % 2 == 0 { home.alice } else { home.bob };
            tokio::spawn(async move { confirm_pair(&db, viewer, uuid(n)).await })
        })
        .collect();
    let mut outcomes = Vec::new();
    for request in racing {
        outcomes.push(request.await.expect("the task ran")?);
    }
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1, "{outcomes:?}");
    // The winner rejected its rivals before it committed, so each loser
    // found its own pair rejected: not one of them saw a half-done state.
    assert_eq!(outcomes.iter().filter(|outcome| **outcome == Err(Refused::NotFound)).count(), 5, "{outcomes:?}");
    let claims = db.query(Query::table("transaction_pairs").index("idx_transaction_pairs_transaction_id_a", KeyRange::eq(["c0"]))).await?;
    let mut statuses = texts_of(&claims, "status");
    statuses.sort();
    assert_eq!(statuses, ["confirmed", "rejected", "rejected", "rejected", "rejected", "rejected"]);
    let winner = outcomes.iter().position(|outcome| outcome.is_ok()).unwrap() as u128 + 1;
    assert_eq!(stored(&db, "transaction_pairs", uuid(winner)).await?.get::<String>("status")?, "confirmed");
    Ok(())
}

/// A recurring charge or income as a caller proposes it.
#[derive(Clone)]
struct Recurring {
    id: Uuid,
    owner: Uuid,
    merchant_key: &'static str,
    merchant_name: &'static str,
    cadence: &'static str,
    is_income: bool,
    members: Vec<&'static str>,
}

impl Recurring {
    /// The hand-written `INSERT`: `created_at`, `updated`, `is_deleted`
    /// and `splits` are left to their defaults.
    fn insert(&self, status: &str) -> Insert {
        Insert::into("recurrings")
            .set("recurring_id", self.id)
            .set("user_id", self.owner)
            .set("merchant_key", self.merchant_key)
            .set("merchant_name", self.merchant_name)
            .set("cadence", self.cadence)
            .set("is_income", self.is_income)
            .set("status", status)
            .set("transaction_ids", self.members.clone())
    }
}

/// The unique key a merchant's recurring is upserted on: one per user,
/// merchant and direction, dismissed and deleted rows included.
const RECURRING_KEY: &str = "recurrings_user_merchant_direction_unique";

/// The user confirms a recurring. A new one is inserted. One already there
/// for the merchant, whatever its state, is confirmed and undeleted, its
/// members become the union of both lists, and its name is replaced only
/// when `rename` says so. Returns the surviving id.
fn confirm_upsert(new: &Recurring, rename: bool) -> Insert {
    new.insert("confirmed")
        .on_conflict_update(RECURRING_KEY)
        .update("status", "confirmed")
        .update("merchant_name", when(val(rename), excluded("merchant_name"), col("merchant_name")))
        .update("transaction_ids", call(Func::ListDistinct, [col("transaction_ids").list_concat(excluded("transaction_ids"))]))
        .update("is_deleted", false)
        .update("updated", now())
        .returning(["recurring_id"])
}

/// The detector suggests a recurring. A row already there for the merchant
/// is touched only if it is a tombstone, which is revived with its old
/// status. Otherwise nothing is written and nothing comes back, which is
/// how the caller knows to leave it be.
fn suggest_upsert(new: &Recurring) -> Insert {
    new.insert("suggested")
        .on_conflict_update(RECURRING_KEY)
        .update_excluded(["cadence", "transaction_ids"])
        .update("is_deleted", false)
        .update("updated", now())
        .update_where(col("is_deleted").eq(true))
        .returning(["recurring_id", "status"])
}

/// Is any of these transactions already a member of another live recurring
/// of the user that was not dismissed?
fn overlapping(owner: Uuid, members: &[&str], except: Option<Uuid>) -> Query {
    let mut rows = Query::table("recurrings")
        .index("idx_recurrings_user_id_updated", KeyRange::eq([owner]))
        .filter(live())
        .filter(col("status").ne("dismissed"))
        .filter(col("transaction_ids").list_overlaps(members.to_vec()));
    if let Some(this) = except {
        rows = rows.filter(col("recurring_id").ne(this));
    }
    rows.columns(["recurring_id"]).limit(1)
}

/// Which of these ids are live transactions of the user (and, for adding to
/// a recurring, settled ones).
fn own_transactions(owner: Uuid, ids: &[&str], settled: bool) -> Query {
    let mut rows = Query::table("transactions").keys(ids.iter().map(|id| [*id])).filter(col("user_id").eq(owner)).filter(live());
    if settled {
        rows = rows.filter(col("pending").eq(false));
    }
    rows.columns(["transaction_id", "amount"])
}

/// Creating a recurring: the members must be the user's live transactions
/// and belong to no other recurring; then the upsert.
async fn create_recurring(db: &Client, new: &Recurring, rename: bool) -> DbResult<Result<Uuid, Refused>> {
    let tx = db.begin().await?;
    if tx.query(own_transactions(new.owner, &new.members, false)).await?.len() != new.members.len() {
        tx.rollback().await?;
        return Ok(Err(Refused::NotFound));
    }
    if tx.query_opt(overlapping(new.owner, &new.members, None)).await?.is_some() {
        tx.rollback().await?;
        return Ok(Err(Refused::Collision));
    }
    let id = tx.insert(confirm_upsert(new, rename)).await?.one()?.get("recurring_id")?;
    tx.commit().await?;
    Ok(Ok(id))
}

/// More members for a recurring of the user's, de-duplicated. Answers the
/// member list after the union.
async fn add_to_recurring(db: &Client, owner: Uuid, recurring: Uuid, members: &[&str]) -> DbResult<Result<Vec<String>, Refused>> {
    let tx = db.begin().await?;
    // The recurring, if it is live and the user's.
    let this = Query::table("recurrings").key([recurring]).filter(col("user_id").eq(owner)).filter(live());
    if tx.query_opt(this.clone().columns(["recurring_id", "is_income", "status"])).await?.is_none()
        || tx.query(own_transactions(owner, members, true)).await?.len() != members.len()
    {
        tx.rollback().await?;
        return Ok(Err(Refused::NotFound));
    }
    if tx.query_opt(overlapping(owner, members, Some(recurring))).await?.is_some() {
        tx.rollback().await?;
        return Ok(Err(Refused::Collision));
    }
    let union = call(Func::ListDistinct, [col("transaction_ids").list_concat(members.to_vec())]);
    tx.update(Update::table("recurrings").key([recurring]).filter(col("user_id").eq(owner)).set("transaction_ids", union).set("updated", now())).await?;
    let now_members = tx.query_one(this.columns(["transaction_ids"])).await?.get("transaction_ids")?;
    tx.commit().await?;
    Ok(Ok(now_members))
}

/// A recurring's row, for asserting on.
async fn recurring_state(db: &impl Db, id: Uuid) -> DbResult<(String, String, Vec<String>, Option<bool>)> {
    let row = stored(db, "recurrings", id).await?;
    Ok((row.get("status")?, row.get("merchant_name")?, row.get("transaction_ids")?, row.get("is_deleted")?))
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

// R-W4, R-Q14, R-T7, R-S3, R-X1: a conflict on a three-column unique key; `CASE` on a parameter;
// array union in the `SET`.
#[tokio::test(flavor = "multi_thread")]
async fn confirming_a_recurring_upserts_on_the_merchant_and_unions_its_members() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    for id in ["n1", "n2", "n3", "n4", "n5", "n6", "n7"] {
        txn(&db, home.alice, id, "acc-1", 15.99, "2026-03-05").await?;
    }
    txn(&db, home.bob, "b1", "acc-2", 15.99, "2026-03-05").await?;
    soft_delete(&db, "transactions", "n7", Scope::Owner(home.alice)).await?;
    let long_ago = ts("2001-01-01T00:00:00Z");
    let netflix = Recurring {
        id: uuid(1),
        owner: home.alice,
        merchant_key: "netflix",
        merchant_name: "Netflix",
        cadence: "monthly",
        is_income: false,
        members: vec!["n1", "n2"],
    };

    // New: the offered id stands, and the defaults fill the rest.
    assert_eq!(create_recurring(&db, &netflix, false).await?, Ok(uuid(1)));
    assert_eq!(recurring_state(&db, uuid(1)).await?, ("confirmed".to_string(), "Netflix".to_string(), strings(&["n1", "n2"]), Some(false)));
    assert_eq!(stored(&db, "recurrings", uuid(1)).await?.get::<Json>("splits")?, json!([]));
    // Members that are someone else's, deleted or unknown: not found.
    for stranger in ["b1", "n7", "n-none"] {
        let with_stranger = Recurring { id: uuid(2), merchant_key: "hulu", members: vec!["n3", stranger], ..netflix.clone() };
        assert_eq!(create_recurring(&db, &with_stranger, false).await?, Err(Refused::NotFound), "{stranger}");
    }
    // A member of another live recurring: refused by the overlap probe.
    let overlapping_hulu = Recurring { id: uuid(2), merchant_key: "hulu", merchant_name: "Hulu", members: vec!["n3", "n2"], ..netflix.clone() };
    assert_eq!(create_recurring(&db, &overlapping_hulu, false).await?, Err(Refused::Collision));
    assert_eq!(db.query(Query::table("recurrings")).await?.len(), 1);

    // The same merchant again, under a new id and another spelling. The
    // row that stands is the first; the members are both lists, in order
    // of first appearance; the name is kept, since `rename` is false;
    // the cadence is not among what a conflict rewrites.
    backdate(&db, "recurrings", uuid(1), long_ago).await?;
    let again = Recurring { id: uuid(3), merchant_name: "NETFLIX.COM", cadence: "weekly", members: vec!["n3", "n4"], ..netflix.clone() };
    assert_eq!(create_recurring(&db, &again, false).await?, Ok(uuid(1)));
    assert_eq!(recurring_state(&db, uuid(1)).await?, ("confirmed".to_string(), "Netflix".to_string(), strings(&["n1", "n2", "n3", "n4"]), Some(false)));
    let row = stored(&db, "recurrings", uuid(1)).await?;
    assert_eq!(row.get::<String>("cadence")?, "monthly");
    assert!(row.get::<Timestamp>("updated")? > long_ago);
    assert!(db.get("recurrings", [uuid(3)]).await?.is_none());
    // The union drops what is already there, wherever it stands in the
    // offered list; with `rename` the offered name replaces the old.
    let returned = db.insert(confirm_upsert(&Recurring { members: vec!["n4", "n5", "n1", "n5"], ..again.clone() }, true)).await?;
    assert_eq!((returned.affected(), returned.one()?.get::<Uuid>("recurring_id")?), (1, uuid(1)));
    assert_eq!(
        recurring_state(&db, uuid(1)).await?,
        ("confirmed".to_string(), "NETFLIX.COM".to_string(), strings(&["n1", "n2", "n3", "n4", "n5"]), Some(false))
    );

    // The user dismisses it; the row stays as the merchant's denylist
    // entry. Confirming the merchant again revives that row.
    assert_eq!(update_status(&update(&db, "recurrings", uuid(1), data! {"status": "dismissed"}, Scope::Owner(home.alice), vec![]).await?), 200);
    assert_eq!(update_status(&update(&db, "recurrings", uuid(1), data! {"status": "dismissed"}, Scope::Owner(home.bob), vec![]).await?), 304);
    assert_eq!(create_recurring(&db, &Recurring { id: uuid(4), members: vec!["n6"], ..netflix.clone() }, false).await?, Ok(uuid(1)));
    assert_eq!(recurring_state(&db, uuid(1)).await?.0, "confirmed");
    // So does a soft-deleted row: its key is still taken, and the upsert
    // undeletes it.
    soft_delete(&db, "recurrings", uuid(1), Scope::Owner(home.alice)).await?;
    assert_eq!(db.insert(confirm_upsert(&netflix, false)).await?.one()?.get::<Uuid>("recurring_id")?, uuid(1));
    assert_eq!(
        recurring_state(&db, uuid(1)).await?,
        ("confirmed".to_string(), "NETFLIX.COM".to_string(), strings(&["n1", "n2", "n3", "n4", "n5", "n6"]), Some(false))
    );
    // A plain insert on the same key is the unique violation the upsert
    // exists to avoid.
    let plain = db.insert(Recurring { id: uuid(5), ..netflix.clone() }.insert("confirmed")).await.unwrap_err();
    assert!(plain.violates(RECURRING_KEY));

    // Direction and owner are part of the key: the same merchant paying
    // the user, or charging another user, is another recurring.
    txn(&db, home.alice, "refund-1", "acc-1", -15.99, "2026-03-05").await?;
    assert_eq!(create_recurring(&db, &Recurring { id: uuid(6), is_income: true, members: vec!["refund-1"], ..netflix.clone() }, false).await?, Ok(uuid(6)));
    assert_eq!(create_recurring(&db, &Recurring { id: uuid(7), owner: home.bob, members: vec!["b1"], ..netflix.clone() }, false).await?, Ok(uuid(7)));
    assert_eq!(db.query(Query::table("recurrings")).await?.len(), 3);
    Ok(())
}

// R-W4, R-W5, R-T9, R-Q2: `DO UPDATE .. WHERE` can decline to write and return nothing; the caller branches on it.
#[tokio::test(flavor = "multi_thread")]
async fn a_suggested_recurring_revives_a_tombstone_but_never_touches_a_live_row() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let long_ago = ts("2001-01-01T00:00:00Z");
    let gym = Recurring { id: uuid(1), owner: ann, merchant_key: "gym", merchant_name: "Gym", cadence: "monthly", is_income: false, members: vec!["g1", "g2"] };
    let offered = Recurring { id: uuid(2), merchant_name: "GYM #42", cadence: "weekly", members: vec!["g1", "g2", "g3"], ..gym.clone() };
    let attempt = |new: &Recurring| {
        let insert = suggest_upsert(new);
        let db = &db;
        async move {
            let rows = db.insert(insert).await?;
            let affected = rows.affected();
            Ok::<_, DbError>((affected, rows.opt().map(|row| (row.get::<Uuid>("recurring_id").unwrap(), row.get::<String>("status").unwrap()))))
        }
    };

    // New: inserted as a suggestion.
    assert_eq!(attempt(&gym).await?, (1, Some((uuid(1), "suggested".to_string()))));
    backdate(&db, "recurrings", uuid(1), long_ago).await?;

    // Live, in any status: the condition declines the update. No row
    // comes back, nothing is counted, and the row is as it was.
    for status in ["suggested", "confirmed", "dismissed"] {
        db.update(Update::table("recurrings").key([uuid(1)]).set("status", status)).await?;
        assert_eq!(attempt(&offered).await?, (0, None), "{status}");
        let row = stored(&db, "recurrings", uuid(1)).await?;
        assert_eq!(
            (row.get::<String>("status")?, row.get::<String>("cadence")?, row.get::<Vec<String>>("transaction_ids")?, row.get::<Timestamp>("updated")?),
            (status.to_string(), "monthly".to_string(), strings(&["g1", "g2"]), long_ago),
            "{status}"
        );
    }
    // A flag that is NULL is not "deleted" either.
    db.update(Update::table("recurrings").key([uuid(1)]).set("is_deleted", Value::Null)).await?;
    assert_eq!(attempt(&offered).await?, (0, None));
    // So the detector, having got nothing back, replaces the member list
    // itself, and only when it differs.
    db.update(Update::table("recurrings").key([uuid(1)]).set("transaction_ids", offered.members.clone()).set("cadence", offered.cadence).set("updated", now()))
        .await?;
    assert_eq!(stored(&db, "recurrings", uuid(1)).await?.get::<Vec<String>>("transaction_ids")?, ["g1", "g2", "g3"]);

    // A tombstone is revived, keeping its id, its name and its status,
    // which the caller reads to decide what comes next: a dismissed
    // merchant stays dismissed, a confirmed one is confirmed again.
    for status in ["suggested", "confirmed", "dismissed"] {
        db.update(
            Update::table("recurrings")
                .key([uuid(1)])
                .set("status", status)
                .set("is_deleted", true)
                .set("transaction_ids", vec!["old"])
                .set("cadence", "yearly")
                .set("updated", long_ago),
        )
        .await?;
        assert_eq!(attempt(&offered).await?, (1, Some((uuid(1), status.to_string()))), "{status}");
        let row = stored(&db, "recurrings", uuid(1)).await?;
        assert_eq!(
            (
                row.get::<Option<bool>>("is_deleted")?,
                row.get::<String>("cadence")?,
                row.get::<Vec<String>>("transaction_ids")?,
                row.get::<String>("merchant_name")?
            ),
            (Some(false), "weekly".to_string(), strings(&["g1", "g2", "g3"]), "Gym".to_string()),
            "{status}"
        );
        assert!(row.get::<Timestamp>("updated")? > long_ago);
    }
    assert!(db.get("recurrings", [uuid(2)]).await?.is_none());

    // The detector reads every row of the user, tombstones and all, to
    // match its groups against.
    db.insert(Recurring { id: uuid(3), merchant_key: "rent", ..gym.clone() }.insert("confirmed").set("is_deleted", true)).await?;
    let bob = user(&db, "bob").await?;
    db.insert(Recurring { id: uuid(4), owner: bob, ..gym.clone() }.insert("suggested")).await?;
    let all = db.query(Query::table("recurrings").index("idx_recurrings_user_id_updated", KeyRange::eq([ann]))).await?;
    let mut ids = column::<Uuid>(&all, "recurring_id");
    ids.sort();
    assert_eq!(ids, [uuid(1), uuid(3)]);
    assert_eq!(all.columns(), all_columns("recurrings"));
    Ok(())
}

// R-Q14, R-T7, R-Q10, R-W6, R-X1: a list of text as a column: overlap probes, append, de-duplicating union,
// `NOT EXISTS .. = ANY(column)`.
#[tokio::test(flavor = "multi_thread")]
async fn recurring_member_lists_are_probed_for_overlap_unioned_appended_and_swept() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    for id in ["m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8"] {
        insert(
            &db,
            "transactions",
            data! {"user_id": home.alice, "transaction_id": id, "account_id": "acc-1", "amount": 30, "date": "2026-03-05", "pending": id == "m7"},
            &[],
            false,
        )
        .await?;
    }
    soft_delete(&db, "transactions", "m8", Scope::Owner(home.alice)).await?;
    txn(&db, home.bob, "b1", "acc-2", 30, "2026-03-05").await?;
    let base = Recurring { id: uuid(0), owner: home.alice, merchant_key: "", merchant_name: "x", cadence: "monthly", is_income: false, members: vec![] };
    let (gym, spotify, old, gone, bobs) = (uuid(1), uuid(2), uuid(3), uuid(4), uuid(5));
    db.insert(Recurring { id: gym, merchant_key: "gym", members: vec!["m1", "m2"], ..base.clone() }.insert("confirmed")).await?;
    db.insert(Recurring { id: spotify, merchant_key: "spotify", members: vec!["m3"], ..base.clone() }.insert("suggested")).await?;
    db.insert(Recurring { id: old, merchant_key: "old", members: vec!["m4"], ..base.clone() }.insert("dismissed")).await?;
    db.insert(Recurring { id: gone, merchant_key: "gone", members: vec!["m5"], ..base.clone() }.insert("confirmed").set("is_deleted", true)).await?;
    db.insert(Recurring { id: bobs, owner: home.bob, merchant_key: "gym", members: vec!["b1", "m6"], ..base.clone() }.insert("confirmed")).await?;
    let probe = |members: &[&str], except: Option<Uuid>| {
        let query = overlapping(home.alice, members, except);
        let db = &db;
        async move { Ok::<_, DbError>(db.query_opt(query).await?.map(|row| row.get::<Uuid>("recurring_id").unwrap())) }
    };

    // Two lists overlap when they share any item. Dismissed and deleted
    // recurrings do not hold on to their members, nor do other users'
    // recurrings; the one being added to is left out by id.
    assert_eq!(probe(&["m9", "m2"], None).await?, Some(gym));
    assert_eq!(probe(&["m3"], None).await?, Some(spotify));
    assert_eq!(probe(&["m4", "m5", "m6", "b1", "m9"], None).await?, None);
    assert_eq!(probe(&[], None).await?, None);
    assert_eq!(probe(&["m1", "m2"], Some(gym)).await?, None);
    assert_eq!(probe(&["m1", "m3"], Some(gym)).await?, Some(spotify));

    // New members join once, after the ones there.
    assert_eq!(add_to_recurring(&db, home.alice, gym, &["m6"]).await?, Ok(strings(&["m1", "m2", "m6"])));
    assert_eq!(add_to_recurring(&db, home.alice, gym, &["m6", "m4", "m1"]).await?, Ok(strings(&["m1", "m2", "m6", "m4"])));
    // Refused: a member of another recurring; a pending, deleted or
    // foreign transaction; a recurring that is deleted or not the user's.
    assert_eq!(add_to_recurring(&db, home.alice, gym, &["m5", "m3"]).await?, Err(Refused::Collision));
    for (recurring, member) in [(gym, "m7"), (gym, "m8"), (gym, "b1"), (gone, "m5"), (bobs, "m5"), (uuid(404), "m5")] {
        assert_eq!(add_to_recurring(&db, home.alice, recurring, &[member]).await?, Err(Refused::NotFound), "{member}");
    }
    assert_eq!(recurring_state(&db, gym).await?.2, ["m1", "m2", "m6", "m4"]);

    // The detector appends by key, without looking: `||` keeps what is
    // already there a second time.
    db.update(Update::table("recurrings").key([gym]).set("transaction_ids", col("transaction_ids").list_concat(vec!["m4", "m5"])).set("updated", now()))
        .await?;
    assert_eq!(recurring_state(&db, gym).await?.2, ["m1", "m2", "m6", "m4", "m4", "m5"]);
    // The next union folds the repeat away.
    db.update(
        Update::table("recurrings").key([gym]).set("transaction_ids", call(Func::ListDistinct, [col("transaction_ids").list_concat(Vec::<String>::new())])),
    )
    .await?;
    assert_eq!(recurring_state(&db, gym).await?.2, ["m1", "m2", "m6", "m4", "m5"]);

    // The defaults of a confirmed recurring, deleted or not. An update
    // first stores them: a JSON array and a date.
    let splits = json!([
        {"amount": 20, "label": {"budget_id": uuid(40).to_string(), "category_id": uuid(50).to_string()}},
        {"amount": 10, "label": {"budget_id": null, "category_id": null}},
    ]);
    assert_eq!(
        update_status(&update(&db, "recurrings", gym, data! {"splits": splits.clone(), "splits_since": "2026-03-01"}, Scope::Owner(home.alice), vec![]).await?),
        200
    );
    let defaults = |id: Uuid| {
        Query::table("recurrings").key([id]).filter(col("user_id").eq(home.alice)).filter(col("status").eq("confirmed")).columns([
            "label_budget_id",
            "label_category_id",
            "transaction_ids",
            "splits",
            "splits_since",
        ])
    };
    let row = db.query_one(defaults(gym)).await?;
    assert_eq!(row.columns(), ["label_budget_id", "label_category_id", "transaction_ids", "splits", "splits_since"]);
    assert_eq!(
        (row.get::<Json>("splits")?, row.get::<Date>("splits_since")?.to_string(), row.get::<Option<Uuid>>("label_category_id")?),
        (splits, "2026-03-01".to_string(), None)
    );
    assert!(db.query_opt(defaults(gone)).await?.is_some());
    assert!(db.query_opt(defaults(spotify)).await?.is_none());
    assert!(db.query_opt(defaults(bobs)).await?.is_none());

    // The hourly sweep soft-deletes the user's recurrings none of whose
    // members is a live transaction of theirs: a member list that is empty,
    // all deleted, or all someone else's.
    let (empty, dead, foreign) = (uuid(6), uuid(7), uuid(8));
    db.insert(Recurring { id: empty, merchant_key: "empty", ..base.clone() }.insert("suggested")).await?;
    db.insert(Recurring { id: dead, merchant_key: "dead", members: vec!["m8", "m-never"], ..base.clone() }.insert("confirmed")).await?;
    db.insert(Recurring { id: foreign, merchant_key: "foreign", members: vec!["b1"], ..base.clone() }.insert("suggested")).await?;
    let sweep = |owner: Uuid| {
        let a_live_member = exists(
            Query::table("transactions")
                .alias("t")
                .index("idx_transactions_user_id_transaction_id", KeyRange::eq([owner]))
                .filter(col("t.transaction_id").in_list(col("r.transaction_ids")).and(live_in("t"))),
        );
        let rows =
            Query::table("recurrings").alias("r").index("idx_recurrings_user_id_updated", KeyRange::eq([owner])).filter(live_in("r").and(a_live_member.not()));
        db.update(Update::rows(rows).set("is_deleted", true).set("updated", now()).returning(["recurring_id"]))
    };
    let swept = sweep(home.alice).await?;
    let mut swept_ids = column::<Uuid>(&swept, "recurring_id");
    swept_ids.sort();
    assert_eq!((swept.affected(), swept_ids), (3, vec![empty, dead, foreign]));
    // Nothing more to sweep; and Bob's, which has a member of his, stays.
    assert_eq!(sweep(home.alice).await?.affected(), 0);
    assert_eq!(sweep(home.bob).await?.affected(), 0);
    // Once its last live member is deleted, a recurring goes too.
    soft_delete(&db, "transactions", "m3", Scope::Owner(home.alice)).await?;
    assert_eq!(column::<Uuid>(&sweep(home.alice).await?, "recurring_id"), [spotify]);
    Ok(())
}

/// The label the user confirmed most often among some of their
/// transactions; on a tie, the one used most recently.
fn most_common_label(owner: Uuid, members: &[String]) -> Query {
    Query::table("transactions")
        .keys(members.iter().map(|id| [id.as_str()]))
        .filter(col("user_id").eq(owner))
        .filter(live())
        .filter(col("label_category_confidence").eq(1).and(col("label_category_id").is_not_null()))
        .group_by([col("label_budget_id"), col("label_category_id")])
        .select("budget_id", col("label_budget_id"))
        .select("category_id", col("label_category_id"))
        .order_by(count().desc())
        .order_by(max(col("date")).desc())
        .limit(1)
}

/// A `(budget, category)` label; either part may be missing.
type Label = (Option<Uuid>, Option<Uuid>);

/// Give a recurring's default label to its member transactions. A row takes
/// it when it is unlabeled, or holds only a suggestion, or holds the
/// recurring's previous default (so it was following it), or was cleared
/// under no budget or the previous one. What the user labeled otherwise is
/// left. A row that already has exactly the new label is not written at
/// all, so its `updated` does not move and no delta is sent for it.
fn propagate_label(owner: Uuid, members: &[String], new: Label, confidence: f64, previous: Label) -> Update {
    let (budget, category, confident) = (|| col("label_budget_id"), || col("label_category_id"), || col("label_category_confidence"));
    let may_take_it = any([
        confident().is_null(),
        confident().gt(0).and(confident().lt(1)),
        confident().ge(1).and(budget().is_not_distinct_from(previous.0)).and(category().is_not_distinct_from(previous.1)),
        confident().le(0).and(budget().is_null().or(budget().eq(previous.0))),
    ]);
    let would_change = any([budget().is_distinct_from(new.0), category().is_distinct_from(new.1), confident().is_distinct_from(confidence)]);
    Update::rows(Query::table("transactions").keys(members.iter().map(|id| [id.as_str()])))
        .filter(col("user_id").eq(owner))
        .filter(live())
        .filter(may_take_it)
        .filter(would_change)
        .set("label_budget_id", new.0)
        .set("label_category_id", new.1)
        .set("label_category_confidence", confidence)
        .set("updated", now())
}

/// The tail of every transaction that confirms or labels a recurring: read
/// the recurring's defaults; if it has no default label yet, adopt the
/// members' most common one; propagate it. Answers how many transactions
/// were relabeled.
async fn label_confirmed_recurring(tx: &Transaction, owner: Uuid, recurring: Uuid, previous: Option<Label>) -> DbResult<u64> {
    let this = || Query::table("recurrings").key([recurring]).filter(col("user_id").eq(owner));
    let Some(defaults) = tx
        .query_opt(this().filter(col("status").eq("confirmed")).columns(["label_budget_id", "label_category_id", "transaction_ids", "splits", "splits_since"]))
        .await?
    else {
        return Ok(0);
    };
    let members: Vec<String> = defaults.get("transaction_ids")?;
    let mut label: Label = (defaults.get("label_budget_id")?, defaults.get("label_category_id")?);
    let previous = previous.unwrap_or(label);
    if label.1.is_none() {
        let Some(common) = tx.query_opt(most_common_label(owner, &members)).await? else { return Ok(0) };
        label = (common.get("budget_id")?, common.get("category_id")?);
        tx.update(Update::rows(this()).set("label_budget_id", label.0).set("label_category_id", label.1).set("updated", now())).await?;
    }
    Ok(tx.update(propagate_label(owner, &members, label, 1.0, previous)).await?.affected())
}

/// Confirming a recurring. `None` when there is no such live recurring of
/// the user's: the transaction is rolled back and the caller says "not
/// found".
async fn confirm_recurring(db: &Client, owner: Uuid, recurring: Uuid) -> DbResult<Option<u64>> {
    let tx = db.begin().await?;
    // Confirmed, if it is live and the user's.
    let confirmed = tx
        .update(
            Update::table("recurrings")
                .key([recurring])
                .filter(col("user_id").eq(owner))
                .filter(live())
                .set("status", "confirmed")
                .set("updated", now())
                .returning(["recurring_id"]),
        )
        .await?;
    if confirmed.affected() == 0 {
        tx.rollback().await?;
        return Ok(None);
    }
    let relabeled = label_confirmed_recurring(&tx, owner, recurring, None).await?;
    tx.commit().await?;
    Ok(Some(relabeled))
}

/// Setting a recurring's default label: read the recurring, store the new
/// default, and when it is confirmed, move the members that followed the
/// old default over to the new one.
async fn set_recurring_label(db: &Client, owner: Uuid, recurring: Uuid, new: Label) -> DbResult<Option<u64>> {
    let tx = db.begin().await?;
    let this = || Query::table("recurrings").key([recurring]).filter(col("user_id").eq(owner));
    let Some(row) = tx.query_opt(this().filter(live()).columns(["label_budget_id", "label_category_id", "status", "transaction_ids"])).await? else {
        tx.rollback().await?;
        return Ok(None);
    };
    let previous: Label = (row.get("label_budget_id")?, row.get("label_category_id")?);
    tx.update(Update::rows(this()).set("label_budget_id", new.0).set("label_category_id", new.1).set("updated", now())).await?;
    let relabeled = if row.get::<String>("status")? == "confirmed" { label_confirmed_recurring(&tx, owner, recurring, Some(previous)).await? } else { 0 };
    tx.commit().await?;
    Ok(Some(relabeled))
}

// R-T9, R-W6, R-W7, R-Q11, R-X1, R-T1: NULL-safe comparisons, a four-branch predicate over a float state,
// no-op suppression.
#[tokio::test(flavor = "multi_thread")]
async fn a_recurrings_label_reaches_the_members_that_follow_it_and_no_others() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let (home_budget, travel_budget) = (uuid(40), uuid(41));
    let (streaming, films, flights) = (uuid(50), uuid(51), uuid(52));
    let long_ago = ts("2001-01-01T00:00:00Z");
    // (id, date, budget, category, confidence, deleted)
    type Labeled = (&'static str, &'static str, Option<Uuid>, Option<Uuid>, Option<f64>, bool);
    let rows: [Labeled; 9] = [
        ("k1", "2026-03-01", Some(home_budget), Some(streaming), Some(1.0), false),
        ("k2", "2026-04-01", Some(home_budget), Some(streaming), Some(1.0), false),
        ("k3", "2026-05-01", Some(home_budget), Some(films), Some(1.0), false),
        ("k4", "2026-06-01", None, None, None, false),
        ("k5", "2026-07-01", Some(home_budget), Some(films), Some(0.6), false),
        ("k6", "2026-08-01", None, None, Some(0.0), false),
        ("k7", "2026-09-01", Some(travel_budget), None, Some(0.0), false),
        ("k8", "2026-10-01", None, None, None, true),
        ("k9", "2026-11-01", None, None, None, false),
    ];
    for (id, date, budget, category, confidence, deleted) in rows {
        insert(
            &db,
            "transactions",
            data! {
                "user_id": home.alice, "transaction_id": id, "account_id": "acc-1", "amount": 15.99, "date": date, "label_budget_id": budget,
                "label_category_id": category, "label_category_confidence": confidence, "is_deleted": deleted,
            },
            &[],
            false,
        )
        .await?;
        backdate(&db, "transactions", id, long_ago).await?;
    }
    let netflix = Recurring {
        id: uuid(1),
        owner: home.alice,
        merchant_key: "netflix",
        merchant_name: "Netflix",
        cadence: "monthly",
        is_income: false,
        members: vec!["k1", "k2", "k3", "k4", "k5", "k6", "k7", "k8"],
    };
    db.insert(netflix.insert("suggested")).await?;
    let labels = || async {
        let rows = db.query(Query::table("transactions").index("idx_transactions_user_id_transaction_id", KeyRange::eq([home.alice]))).await?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.get::<Option<Uuid>>("label_category_id")?,
                    row.get::<Option<f64>>("label_category_confidence")?,
                    row.get::<Timestamp>("updated")? > long_ago,
                ))
            })
            .collect::<DbResult<Vec<_>>>()
    };

    // The most common label on its own: streaming was confirmed twice,
    // films once. On a tie the label used most recently wins. Unconfirmed,
    // deleted and other users' rows have no say.
    let common = |members: &[&str]| {
        let query = most_common_label(home.alice, &strings(members));
        let db = &db;
        async move {
            Ok::<_, DbError>(db.query_opt(query).await?.map(|row| (row.get::<Option<Uuid>>("budget_id").unwrap(), row.get::<Uuid>("category_id").unwrap())))
        }
    };
    assert_eq!(common(&netflix.members).await?, Some((Some(home_budget), streaming)));
    assert_eq!(common(&["k1", "k3", "k5", "k4"]).await?, Some((Some(home_budget), films)));
    assert_eq!(common(&["k4", "k5", "k6", "k8"]).await?, None);
    assert_eq!(common(&[]).await?, None);

    // Confirming a recurring someone else owns, or one that is not there,
    // finds no row and changes nothing.
    assert_eq!(confirm_recurring(&db, home.bob, uuid(1)).await?, None);
    assert_eq!(confirm_recurring(&db, home.alice, uuid(404)).await?, None);
    assert_eq!(stored(&db, "recurrings", uuid(1)).await?.get::<String>("status")?, "suggested");

    // Alice confirms it. It has no default yet, so the members' most
    // common label becomes the default, and is propagated: to the
    // unlabeled k4, the merely suggested k5 and the cleared k6. Not to
    // k3, which the user labeled otherwise; nor k7, cleared under another
    // budget; nor the deleted k8; nor k9, which is no member. k1 and k2
    // already have exactly this label and are not written at all.
    assert_eq!(confirm_recurring(&db, home.alice, uuid(1)).await?, Some(3));
    let default = stored(&db, "recurrings", uuid(1)).await?;
    assert_eq!(
        (default.get::<String>("status")?, default.get::<Option<Uuid>>("label_budget_id")?, default.get::<Option<Uuid>>("label_category_id")?),
        ("confirmed".to_string(), Some(home_budget), Some(streaming))
    );
    assert_eq!(
        labels().await?,
        [
            (Some(streaming), Some(1.0), false),
            (Some(streaming), Some(1.0), false),
            (Some(films), Some(1.0), false),
            (Some(streaming), Some(1.0), true),
            (Some(streaming), Some(1.0), true),
            (Some(streaming), Some(1.0), true),
            (None, Some(0.0), false),
            (None, None, false),
            (None, None, false),
        ]
    );
    // Confirming again finds every follower labeled: nothing is written.
    assert_eq!(confirm_recurring(&db, home.alice, uuid(1)).await?, Some(0));

    // The default moves to flights under the travel budget. Every
    // member that carried the old default follows; k3 still does not;
    // k7, cleared under the travel budget, is not under the old default
    // and stays cleared.
    for id in ["k4", "k5", "k6"] {
        backdate(&db, "transactions", id, long_ago).await?;
    }
    assert_eq!(set_recurring_label(&db, home.alice, uuid(1), (Some(travel_budget), Some(flights))).await?, Some(5));
    let after: Vec<Option<Uuid>> = labels().await?.into_iter().map(|(category, _, _)| category).collect();
    assert_eq!(after, [Some(flights), Some(flights), Some(films), Some(flights), Some(flights), Some(flights), None, None, None]);
    assert_eq!(stored(&db, "transactions", "k1").await?.get::<Option<Uuid>>("label_budget_id")?, Some(travel_budget));
    // Storing the same default again: the previous default's budget is
    // now the one k7 was cleared under, so k7 follows. The members that
    // already carry the label are not written and not stamped.
    for id in ["k1", "k2", "k4", "k5", "k6"] {
        backdate(&db, "transactions", id, long_ago).await?;
    }
    assert_eq!(set_recurring_label(&db, home.alice, uuid(1), (Some(travel_budget), Some(flights))).await?, Some(1));
    let stamped: Vec<bool> = labels().await?.into_iter().map(|(_, _, stamped)| stamped).collect();
    assert_eq!(stamped, [false, false, false, false, false, false, true, false, false]);
    let k7 = stored(&db, "transactions", "k7").await?;
    assert_eq!((k7.get::<Option<Uuid>>("label_category_id")?, k7.get::<f64>("label_category_confidence")?), (Some(flights), 1.0));
    // And once more: nobody is left to relabel, and nobody is stamped.
    backdate(&db, "transactions", "k7", long_ago).await?;
    assert_eq!(set_recurring_label(&db, home.alice, uuid(1), (Some(travel_budget), Some(flights))).await?, Some(0));
    assert!(labels().await?.iter().all(|(_, _, stamped)| !stamped));
    // A new default moves all six followers; k3 keeps the user's choice.
    assert_eq!(set_recurring_label(&db, home.alice, uuid(1), (Some(travel_budget), Some(streaming))).await?, Some(6));
    assert_eq!(stored(&db, "transactions", "k3").await?.get::<Option<Uuid>>("label_category_id")?, Some(films));
    assert_eq!(set_recurring_label(&db, home.bob, uuid(1), (None, None)).await?, None);
    Ok(())
}

/// One of a recurring's default splits: an amount and, perhaps, a label.
type DefaultSplit = (Decimal, Option<Uuid>, Option<Uuid>);

/// Give each of some transactions the recurring's default splits, unless it
/// is small, old, deleted or already split. In SQL one `INSERT .. SELECT`
/// would cross-join the transactions with three parallel array parameters.
/// Here the eligible transactions are read, and their splits go in as one
/// multi-row insert; the caller runs both in its writing transaction. Each
/// row's id is the server's; its signed amount and its label state are
/// computed by the server as the row goes in.
async fn create_default_splits(db: &impl Db, owner: Uuid, members: &[&str], splits: &[DefaultSplit], larger_than: Decimal, since: &str) -> DbResult<u64> {
    let already_split = exists(
        Query::table("split_transactions")
            .alias("x")
            .index("idx_split_transactions_transaction_id", KeyRange::eq([col("t.transaction_id")]))
            .filter(col("x.user_id").eq(col("t.user_id")).and(live_in("x"))),
    );
    let eligible = Query::table("transactions")
        .alias("t")
        .keys(members.iter().map(|id| [*id]))
        .filter(all([
            col("t.user_id").eq(owner),
            live_in("t"),
            call(Func::Abs, [col("t.amount")]).gt(larger_than),
            col("t.date").ge(since),
            already_split.not(),
        ]))
        .select("user_id", col("t.user_id"))
        .select("transaction_id", col("t.transaction_id"))
        .select("account_id", col("t.account_id"))
        .select("amount", col("t.amount"))
        .select("date", col("t.date"));
    let eligible = db.query(eligible).await?;
    if eligible.is_empty() || splits.is_empty() {
        return Ok(0);
    }
    let mut insert = Insert::into("split_transactions").columns([
        "user_id",
        "transaction_id",
        "account_id",
        "amount",
        "date",
        "label_budget_id",
        "label_category_id",
        "label_category_confidence",
    ]);
    for parent in &eligible {
        for (amount, budget, category) in splits {
            insert = insert.values([
                val(parent.get::<Uuid>("user_id")?),
                val(parent.get::<String>("transaction_id")?),
                val(parent.get::<String>("account_id")?),
                val(*amount) * call(Func::Sign, [val(parent.get::<Decimal>("amount")?)]),
                val(parent.get::<Date>("date")?),
                val(*budget),
                val(*category),
                case([(val(*category).is_not_null(), val(1)), (val(*budget).is_not_null(), val(0))], null()),
            ]);
        }
    }
    Ok(db.insert(insert).await?.affected())
}

// R-W2, R-W3, R-T5, R-S5, R-Q10, R-Q16, R-X1: transactions crossed with three parallel arrays, signed amounts in
// exact decimal, a state column from `CASE`, an anti-join, a uuid per row.
#[tokio::test(flavor = "multi_thread")]
async fn default_splits_are_created_once_per_eligible_transaction_with_signed_amounts() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    // (id, account, amount, date)
    let rows: [(&str, &str, Value, &str); 11] = [
        ("s01", "acc-1", 100.into(), "2026-03-10"),
        ("s02", "acc-2", (-100).into(), "2026-03-11"),
        ("s03", "acc-1", 100.into(), "2026-03-01"),
        ("s04", "acc-1", 100.into(), "2026-03-12"),
        ("s05", "acc-1", 100.into(), "2026-03-12"),
        // Not eligible: too small (the bound is exclusive), no amount,
        // too old, deleted, already split.
        ("s06", "acc-1", 9.99.into(), "2026-03-12"),
        ("s07", "acc-1", (-10).into(), "2026-03-12"),
        ("s08", "acc-1", Value::Null, "2026-03-12"),
        ("s09", "acc-1", 100.into(), "2026-02-28"),
        ("s10", "acc-1", 100.into(), "2026-03-12"),
        ("s11", "acc-1", 100.into(), "2026-03-12"),
    ];
    for (id, account, amount, date) in rows {
        txn(&db, home.alice, id, account, amount, date).await?;
    }
    txn(&db, home.bob, "s-bob", "acc-9", 100, "2026-03-12").await?;
    soft_delete(&db, "transactions", "s10", Scope::Owner(home.alice)).await?;
    // s11 has a live split. s04 has only a deleted one, and s05 one that
    // belongs to another user: neither counts as "already split".
    for (parent, owner, deleted) in [("s11", home.alice, false), ("s04", home.alice, true), ("s05", home.bob, false)] {
        insert(
            &db,
            "split_transactions",
            data! {
                "user_id": owner, "transaction_id": parent, "account_id": "acc-1", "amount": 1, "date": "2026-03-12", "custom_name": "by hand",
                "is_deleted": deleted,
            },
            &[],
            false,
        )
        .await?;
    }
    let (budget, category) = (uuid(40), uuid(50));
    let defaults: Vec<DefaultSplit> = vec![(dec("33.33"), Some(budget), Some(category)), (dec("41.67"), Some(budget), None), (dec("25"), None, None)];
    let members = ["s01", "s02", "s03", "s04", "s05", "s06", "s07", "s08", "s09", "s10", "s11", "s-bob", "s-none"];

    let created = db
        .exclusive(|tx| async move {
            create_default_splits(
                &tx,
                home.alice,
                &members,
                &defaults,
                dec("10"),
                "2026-03-01",
            )
            .await
        })
        .await?;
    assert_eq!(created, 5 * 3);

    let made = db
        .query(
            Query::table("split_transactions")
                .filter(col("custom_name").eq(""))
                .order_by(col("transaction_id"))
                .order_by(call(Func::Abs, [col("amount")]).desc()),
        )
        .await?;
    assert_eq!(made.len(), 15);
    assert_eq!(texts_of(&made, "transaction_id"), ["s01", "s01", "s01", "s02", "s02", "s02", "s03", "s03", "s03", "s04", "s04", "s04", "s05", "s05", "s05"]);
    // Every row has an id of its own, from the column's default.
    assert_eq!(column::<Uuid>(&made, "split_transaction_id").into_iter().collect::<BTreeSet<_>>().len(), 15);
    // The amounts carry the parent's sign, to the cent.
    let amounts: Vec<String> = column::<Decimal>(&made, "amount").iter().map(Decimal::to_string).collect();
    assert_eq!(amounts[..6], ["41.67", "33.33", "25.00", "-41.67", "-33.33", "-25.00"]);
    // A category means a confirmed label, a budget alone a cleared one,
    // neither means unlabeled.
    assert_eq!(column::<Option<f64>>(&made, "label_category_confidence")[..3], [Some(0.0), Some(1.0), None]);
    assert_eq!(column::<Option<Uuid>>(&made, "label_category_id")[..3], [None, Some(category), None]);
    assert_eq!(column::<Option<Uuid>>(&made, "label_budget_id")[..3], [Some(budget), Some(budget), None]);
    // The rest is the parent's, or a default: one statement, one clock.
    let of_refund = &made.iter().collect::<Vec<_>>()[3];
    assert_eq!(
        (
            of_refund.get::<String>("account_id")?,
            of_refund.get::<Date>("date")?.to_string(),
            of_refund.get::<Uuid>("user_id")?,
            of_refund.get::<Option<bool>>("is_deleted")?
        ),
        ("acc-2".to_string(), "2026-03-11".to_string(), home.alice, Some(false))
    );
    assert_eq!(column::<Timestamp>(&made, "updated").into_iter().collect::<BTreeSet<_>>().len(), 1);
    // Each transaction's splits add up to it exactly.
    let totals = db
        .query(
            Query::table("split_transactions")
                .filter(col("custom_name").eq(""))
                .group_by([col("transaction_id")])
                .select("transaction_id", col("transaction_id"))
                .select("total", sum(col("amount")))
                .order_by(col("transaction_id")),
        )
        .await?;
    assert_eq!(column::<Decimal>(&totals, "total").iter().map(Decimal::to_string).collect::<Vec<_>>(), ["100.00", "-100.00", "100.00", "100.00", "100.00"]);

    // Run again, every one of them is already split: nothing is added.
    let defaults = vec![(dec("50"), None, None), (dec("50"), None, None)];
    let again = db
        .exclusive(|tx| async move {
            create_default_splits(
                &tx,
                home.alice,
                &["s01", "s02", "s03", "s04", "s11"],
                &defaults,
                dec("10"),
                "2026-03-01",
            )
            .await
        })
        .await?;
    assert_eq!(again, 0);
    // s05's only live split was Bob's; Alice's own now stand beside it.
    assert_eq!(db.query(Query::table("split_transactions").index("idx_split_transactions_transaction_id", KeyRange::eq(["s05"]))).await?.len(), 4);
    assert_eq!(db.query(Query::table("split_transactions")).await?.len(), 18);
    Ok(())
}

// R-W6, R-W7, R-Q1, R-Q2: a split's own statements: the probe for a live split, the move to another parent, the
// edit and the soft deletes.
#[tokio::test(flavor = "multi_thread")]
async fn splits_are_probed_edited_repointed_and_deleted_with_their_parent() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let mut ids = Vec::new();
    for (parent, owner, account) in [
        ("pending-1", home.alice, "acc-1"),
        ("pending-1", home.alice, "acc-1"),
        ("other", home.alice, "acc-1"),
        ("bobs", home.bob, "acc-2"),
        ("carols", home.carol, "acc-3"),
        ("pending-1", home.carol, "acc-3"),
    ] {
        ids.push(
            insert(
                &db,
                "split_transactions",
                data! {"user_id": owner, "transaction_id": parent, "account_id": account, "amount": 5, "date": "2026-03-05"},
                &[],
                false,
            )
            .await?
            .one()?
            .get::<Uuid>("split_transaction_id")?,
        );
    }
    assert!(soft_delete(&db, "split_transactions", ids[1], Scope::Owner(home.alice)).await?);
    let long_ago = ts("2001-01-01T00:00:00Z");
    for id in &ids {
        backdate(&db, "split_transactions", *id, long_ago).await?;
    }

    // Does this transaction have a live split of the user's?
    let has_split = |owner: Uuid, parent: &'static str| {
        let db = &db;
        async move {
            Ok::<_, DbError>(
                db.query_opt(
                    Query::table("split_transactions")
                        .index("idx_split_transactions_transaction_id", KeyRange::eq([parent]))
                        .filter(col("user_id").eq(owner))
                        .filter(live())
                        .select("one", val(1))
                        .limit(1),
                )
                .await?
                .is_some(),
            )
        }
    };
    assert!(has_split(home.alice, "pending-1").await?);
    assert!(!has_split(home.alice, "bobs").await?);
    assert!(!has_split(home.bob, "pending-1").await?);
    assert!(!has_split(home.alice, "posted-1").await?);

    // The pending transaction was replaced by a posted one. The user's live
    // splits follow it; the count is the answer. The deleted split and
    // another user's split on the same parent stay behind.
    let repoint = |owner: Uuid, from: &'static str, to: &'static str| {
        db.update(
            Update::table("split_transactions")
                .index("idx_split_transactions_transaction_id", KeyRange::eq([from]))
                .filter(col("user_id").eq(owner))
                .filter(live())
                .set("transaction_id", to)
                .set("updated", now()),
        )
    };
    assert_eq!(repoint(home.alice, "pending-1", "posted-1").await?.affected(), 1);
    assert_eq!(repoint(home.alice, "pending-1", "posted-1").await?.affected(), 0);
    assert!(has_split(home.alice, "posted-1").await? && !has_split(home.alice, "pending-1").await?);
    let parents: Vec<String> = db.query(Query::table("split_transactions").keys(ids.iter().map(|id| [*id])).columns(["transaction_id"])).await?.column()?;
    assert_eq!(parents, ["posted-1", "pending-1", "other", "bobs", "carols", "pending-1"]);
    assert!(stored(&db, "split_transactions", ids[0]).await?.get::<Timestamp>("updated")? > long_ago);

    // An edit within the group, and outside it.
    assert_eq!(
        update_status(
            &update(&db, "split_transactions", ids[3], data! {"amount": 7.5, "custom_name": "half", "label_memo": "shared"}, Scope::Group(home.alice), vec![])
                .await?
        ),
        200
    );
    assert_eq!(update_status(&update(&db, "split_transactions", ids[4], data! {"amount": 7.5}, Scope::Group(home.alice), vec![]).await?), 304);
    assert_eq!(update_status(&update(&db, "split_transactions", ids[1], data! {"amount": 7.5}, Scope::Group(home.alice), vec![]).await?), 304);
    let edited = stored(&db, "split_transactions", ids[3]).await?;
    assert_eq!(
        (edited.get::<Decimal>("amount")?, edited.get::<String>("custom_name")?, edited.get::<Uuid>("user_id")?),
        (dec("7.5"), "half".to_string(), home.bob)
    );

    // The splits of deleted transactions, or of an account, go with them;
    // some are deleted by id. All within the group.
    assert_eq!(
        bulk_soft_delete_by_column(&db, "split_transactions", "transaction_id", vec!["posted-1", "carols", "nobodys"], Scope::Group(home.bob)).await?,
        1
    );
    assert_eq!(bulk_soft_delete_by_column(&db, "split_transactions", "account_id", "acc-3", Scope::Group(home.alice)).await?, 0);
    assert_eq!(bulk_soft_delete(&db, "split_transactions", vec![ids[2].into(), ids[3].into(), ids[4].into()], Scope::Group(home.alice)).await?, 2);
    let flags: Vec<Option<bool>> = db.query(Query::table("split_transactions").keys(ids.iter().map(|id| [*id])).columns(["is_deleted"])).await?.column()?;
    assert_eq!(flags, [Some(true), Some(true), Some(true), Some(true), Some(false), Some(false)]);
    Ok(())
}

// R-S6, R-Q1, R-Q6, R-W1, R-W11: budgets, sections and categories: foreign keys are relied on at insert time, and a
// child takes its parent's owner.
#[tokio::test(flavor = "multi_thread")]
async fn sections_and_categories_need_a_live_parent_that_the_group_owns() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let new_id = |rows: Rows, column: &str| rows.one()?.get::<Uuid>(column);
    let budget = new_id(
        insert(&db, "budgets", data! {"user_id": home.alice, "name": "Home", "capacities": json!([{"amount": 2000}])}, &["*"], false).await?,
        "budget_id",
    )?;
    let carols = new_id(insert(&db, "budgets", data! {"user_id": home.carol, "name": "Carol's"}, &["*"], false).await?, "budget_id")?;

    // A child is created under a parent that is live and the group's, and
    // takes the parent's owner, whoever asks.
    let add_section = |viewer: Uuid, budget: Uuid, name: &'static str| {
        let db = &db;
        async move {
            let Some(parent) = find_one(db, Query::table("budgets").key([budget]), &Filters::new(Scope::Group(viewer))).await? else {
                return Ok::<_, DbError>(None);
            };
            let row = insert(
                db,
                "sections",
                data! {"user_id": parent.get::<Uuid>("user_id")?, "budget_id": budget, "name": name, "roll_over": true, "roll_over_start_date": "2026-01-01"},
                &["*"],
                false,
            )
            .await?
            .one()?;
            Ok(Some((row.get::<Uuid>("section_id")?, row.get::<Uuid>("user_id")?)))
        }
    };
    let add_category = |viewer: Uuid, section: Uuid, name: &'static str| {
        let db = &db;
        async move {
            let Some(parent) = find_one(db, Query::table("sections").key([section]), &Filters::new(Scope::Group(viewer))).await? else {
                return Ok::<_, DbError>(None);
            };
            let row =
                insert(db, "categories", data! {"user_id": parent.get::<Uuid>("user_id")?, "section_id": section, "name": name}, &["*"], false).await?.one()?;
            Ok(Some(row.get::<Uuid>("category_id")?))
        }
    };
    let (bills, owner) = add_section(home.bob, budget, "Bills").await?.expect("the group's budget");
    assert_eq!(owner, home.alice);
    let (fun, _) = add_section(home.alice, budget, "Fun").await?.expect("her own budget");
    let rent = add_category(home.bob, bills, "Rent").await?.expect("the group's section");
    let power = add_category(home.alice, bills, "Power").await?.expect("her own section");
    let films = add_category(home.alice, fun, "Films").await?.expect("her own section");
    assert_eq!(stored(&db, "categories", rent).await?.get::<Uuid>("user_id")?, home.alice);
    // Outside the group, or under a deleted parent, there is no parent.
    assert_eq!(add_section(home.alice, carols, "Mine?").await?, None);
    assert_eq!(add_category(home.carol, bills, "Mine?").await?, None);
    assert_eq!(add_section(home.alice, uuid(404), "Lost").await?, None);

    // The database checks the parent too: a child cannot name a budget
    // or a section that does not exist. A deleted parent does exist.
    assert_eq!(code(insert(&db, "sections", data! {"user_id": home.alice, "budget_id": uuid(404)}, &[], false).await), ErrorCode::ForeignKeyViolation);
    assert_eq!(code(insert(&db, "categories", data! {"user_id": home.alice, "section_id": budget}, &[], false).await), ErrorCode::ForeignKeyViolation);
    assert_eq!(code(insert(&db, "sections", data! {"user_id": home.alice}, &[], false).await), ErrorCode::NotNullViolation);

    // The group's rows, all of them or one parent's.
    let group = Filters::new(Scope::Group(home.bob));
    let names = |rows: Rows| {
        let mut names = texts_of(&rows, "name");
        names.sort();
        names
    };
    assert_eq!(names(db.query(find(Query::table("budgets"), &group)).await?), ["Home"]);
    assert_eq!(names(db.query(find(Query::table("sections").index("idx_sections_budget_id", KeyRange::eq([budget])), &group)).await?), ["Bills", "Fun"]);
    assert_eq!(names(db.query(find(Query::table("categories").index("idx_categories_section_id", KeyRange::eq([bills])), &group)).await?), ["Power", "Rent"]);
    assert_eq!(names(db.query(find(Query::table("categories"), &group)).await?), ["Films", "Power", "Rent"]);
    assert!(db
        .query(find(Query::table("sections").index("idx_sections_budget_id", KeyRange::eq([budget])), &Filters::new(Scope::Group(home.carol))))
        .await?
        .is_empty());
    let section = find_one(&db, Query::table("sections").key([bills]), &group).await?.expect("the section");
    assert_eq!((section.get::<bool>("roll_over")?, section.get::<Date>("roll_over_start_date")?.to_string()), (true, "2026-01-01".to_string()));

    // One category is soft-deleted. Read with tombstones, the group still
    // owns it, which is what a label naming it is checked against; the live
    // rows by id no longer have it. Both tables are asked at once.
    assert!(soft_delete(&db, "categories", power, Scope::Group(home.bob)).await?);
    assert!(!soft_delete(&db, "categories", power, Scope::Group(home.carol)).await?);
    let owned = |table: &'static str, ids: Vec<Uuid>| {
        db.query(find(Query::table(table).keys(ids.into_iter().map(|id| [id])), &Filters::new(Scope::Group(home.alice)).with_tombstones()))
    };
    let (budgets, categories) = tokio::join!(owned("budgets", vec![budget, carols, uuid(404)]), owned("categories", vec![films, power, uuid(404)]));
    assert_eq!(column::<Uuid>(&budgets?, "budget_id"), [budget]);
    assert_eq!(column::<Uuid>(&categories?, "category_id"), [films, power]);
    let live_ones = find_by_ids(&db, "categories", vec![films.into(), power.into(), rent.into()], Scope::Group(home.alice)).await?;
    assert_eq!(live_ones.iter().map(|row| row.get("category_id")).collect::<DbResult<Vec<Uuid>>>()?, [films, rent]);
    // A child can still be created under a section whose sibling went.
    assert!(add_category(home.alice, bills, "Water").await?.is_some());
    // Under a deleted section the caller finds no parent. The foreign key
    // would not have minded: a soft-deleted parent is still a row.
    assert!(soft_delete(&db, "sections", fun, Scope::Group(home.alice)).await?);
    assert_eq!(add_category(home.alice, fun, "Games").await?, None);
    insert(&db, "categories", data! {"user_id": home.alice, "section_id": fun, "name": "Games"}, &[], false).await?;
    Ok(())
}

/// Deleting a budget: the sections are read first, on the pool; then one
/// transaction soft-deletes their categories, the sections and the budget,
/// whose row count is the caller's answer. `interrupted` makes a statement
/// in the middle fail, as a lost connection or a bug would.
async fn delete_budget(db: &Client, viewer: Uuid, budget: Uuid, interrupted: bool) -> DbResult<bool> {
    let scope = Scope::Group(viewer);
    let sections: Vec<Uuid> = db
        .query(find(Query::table("sections").index("idx_sections_budget_id", KeyRange::eq([budget])), &Filters::new(scope)).columns(["section_id"]))
        .await?
        .column()?;
    db.exclusive(|tx| async move {
        for section in &sections {
            bulk_soft_delete_by_column(&tx, "categories", "section_id", *section, scope).await?;
        }
        bulk_soft_delete(&tx, "sections", sections.iter().map(|section| Value::from(*section)).collect(), scope).await?;
        if interrupted {
            insert(&tx, "sections", data! {"user_id": viewer, "budget_id": uuid(404)}, &[], false).await?;
        }
        Ok(bulk_soft_delete(&tx, "budgets", vec![budget.into()], scope).await? > 0)
    })
    .await
}

// R-X1, R-X2, R-X7, R-W6, R-S6: a transaction that fails leaves nothing; the cascades the application relies on are
// its own soft-delete fan-outs.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_budget_soft_deletes_its_sections_and_categories_or_nothing_at_all() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let long_ago = ts("2001-01-01T00:00:00Z");
    let mut tree = Vec::new();
    for name in ["Home", "Trips"] {
        let budget: Uuid = insert(&db, "budgets", data! {"user_id": home.alice, "name": name}, &[], false).await?.one()?.get("budget_id")?;
        backdate(&db, "budgets", budget, long_ago).await?;
        let mut sections = Vec::new();
        for _ in 0..2 {
            let section: Uuid = insert(&db, "sections", data! {"user_id": home.alice, "budget_id": budget}, &[], false).await?.one()?.get("section_id")?;
            backdate(&db, "sections", section, long_ago).await?;
            let mut categories = Vec::new();
            for _ in 0..2 {
                let category: Uuid =
                    insert(&db, "categories", data! {"user_id": home.alice, "section_id": section}, &[], false).await?.one()?.get("category_id")?;
                backdate(&db, "categories", category, long_ago).await?;
                categories.push(category);
            }
            sections.push((section, categories));
        }
        tree.push((budget, sections));
    }
    let (home_budget, trips) = (tree[0].0, tree[1].0);
    // A transaction labeled with a category of the budget about to go.
    insert(
        &db,
        "transactions",
        data! {
            "user_id": home.alice, "transaction_id": "t-1", "account_id": "acc-1", "date": "2026-03-05", "label_budget_id": home_budget,
            "label_category_id": tree[0].1[0].1[0], "label_category_confidence": 1,
        },
        &[],
        false,
    )
    .await?;
    // How many live rows each table has, and how many were ever stamped.
    let counts = || async {
        let mut live_rows = Vec::new();
        let mut stamped = 0;
        for table in ["budgets", "sections", "categories"] {
            let rows = db.query(Query::table(table)).await?;
            live_rows.push(rows.iter().filter(|row| row.get::<Option<bool>>("is_deleted").unwrap() != Some(true)).count());
            stamped += rows.iter().filter(|row| row.get::<Timestamp>("updated").unwrap() > long_ago).count();
        }
        Ok::<_, DbError>((live_rows, stamped))
    };
    assert_eq!(counts().await?, (vec![2, 4, 8], 0));

    // Someone outside the group deletes nothing: the row count is zero,
    // which the caller answers with "not found".
    assert!(!delete_budget(&db, home.carol, home_budget, false).await?);
    assert_eq!(counts().await?, (vec![2, 4, 8], 0));

    // The cascade fails after the categories and the sections were
    // flagged. The error reaches the caller, and none of it is kept.
    assert_eq!(code(delete_budget(&db, home.bob, home_budget, true).await), ErrorCode::ForeignKeyViolation);
    assert_eq!(counts().await?, (vec![2, 4, 8], 0));
    // The same holds when the process just stops: a transaction that is
    // dropped is rolled back, and the next writer is let in.
    let tx = db.begin().await?;
    assert_eq!(bulk_soft_delete_by_column(&tx, "categories", "section_id", tree[0].1[0].0, Scope::Group(home.bob)).await?, 2);
    assert_eq!(tx.query(find(Query::table("categories"), &Filters::new(Scope::Group(home.alice)))).await?.len(), 6);
    // Nobody else sees the flags meanwhile.
    assert_eq!(db.query(find(Query::table("categories"), &Filters::new(Scope::Group(home.alice)))).await?.len(), 8);
    drop(tx);
    assert_eq!(counts().await?, (vec![2, 4, 8], 0));

    // Whole, the cascade flags the budget, its two sections and their
    // four categories, all with the one timestamp of their transaction.
    assert!(delete_budget(&db, home.bob, home_budget, false).await?);
    assert_eq!(counts().await?, (vec![1, 2, 4], 7));
    let mut stamps = BTreeSet::new();
    stamps.insert(stored(&db, "budgets", home_budget).await?.get::<Timestamp>("updated")?);
    for (section, categories) in &tree[0].1 {
        stamps.insert(stored(&db, "sections", *section).await?.get::<Timestamp>("updated")?);
        for category in categories {
            let row = stored(&db, "categories", *category).await?;
            assert_eq!(row.get::<Option<bool>>("is_deleted")?, Some(true));
            stamps.insert(row.get::<Timestamp>("updated")?);
        }
    }
    assert_eq!(stamps.len(), 1);
    // Deliberately not cascaded: the label that names the budget stays.
    let labeled = stored(&db, "transactions", "t-1").await?;
    assert_eq!((labeled.get::<Uuid>("label_budget_id")?, labeled.get::<Uuid>("label_category_id")?), (home_budget, tree[0].1[0].1[0]));
    // Deleting it again finds no live section, and flags the budget
    // once more: a soft delete has no `LIVE` guard.
    assert!(delete_budget(&db, home.alice, home_budget, false).await?);
    assert_eq!(counts().await?.0, [1, 2, 4]);

    // Deleting a section: its categories, then the section itself.
    let (section, _) = tree[1].1[0];
    let deleted = db
        .transaction(|tx| async move {
            let categories = bulk_soft_delete_by_column(&tx, "categories", "section_id", section, Scope::Group(home.alice)).await?;
            let sections = bulk_soft_delete(&tx, "sections", vec![section.into()], Scope::Group(home.alice)).await?;
            Ok::<_, DbError>((categories, sections))
        })
        .await?;
    assert_eq!(deleted, (2, 1));
    assert_eq!(counts().await?.0, [1, 1, 2]);
    assert!(find_one(&db, Query::table("budgets").key([trips]), &Filters::new(Scope::Group(home.bob))).await?.is_some());
    Ok(())
}

/// What a write onto an existing account-balance snapshot may rewrite:
/// never its owner, its kind or its account.
const BALANCE_SNAPSHOT_COLUMNS: &[&str] =
    &["snapshot_date", "is_deleted", "balances_available", "balances_current", "balances_limit", "balances_iso_currency_code"];

/// The same for a holding snapshot.
const HOLDING_SNAPSHOT_COLUMNS: &[&str] =
    &["snapshot_date", "is_deleted", "holding_security_id", "institution_price", "institution_value", "cost_basis", "quantity"];

// R-W4, R-W6, R-T5, R-T9, R-Q1: snapshots keep one row per entity and day; a write onto a tombstoned id revives it.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_upserts_keep_one_row_per_day_and_revive_tombstones() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let write = |row: Data, allow: Option<&'static [&'static str]>| upsert(&db, "snapshots", row, allow, None);

    // The id is `<account>-<YYYYMMDD>`, so a second sync on the same day
    // rewrites the day's row.
    let balance = |owner: Uuid, account: &'static str, at: &'static str, current: f64| {
        data! {
            "snapshot_id": "acc-1-20260305", "user_id": owner, "snapshot_date": at, "snapshot_type": "account_balance", "account_id": account,
            "is_deleted": false, "balances_available": current - 10.0, "balances_current": current, "balances_limit": Value::Null,
            "balances_iso_currency_code": "USD",
        }
    };
    let morning = write(balance(home.alice, "acc-1", "2026-03-05T08:00:00Z", 1200.5), Some(BALANCE_SNAPSHOT_COLUMNS)).await?.one()?;
    assert_eq!(
        (morning.get::<Decimal>("balances_current")?, morning.get::<Decimal>("balances_available")?, morning.get::<Option<bool>>("is_deleted")?),
        (dec("1200.50"), dec("1190.50"), Some(false))
    );
    let evening = write(balance(home.alice, "acc-1", "2026-03-05T20:00:00Z", 1100.0), Some(BALANCE_SNAPSHOT_COLUMNS)).await?.one()?;
    assert_eq!((evening.get::<Decimal>("balances_current")?, evening.get::<Timestamp>("snapshot_date")?), (dec("1100"), ts("2026-03-05T20:00:00Z")));
    assert_eq!(db.query(Query::table("snapshots")).await?.len(), 1);
    // The same id arriving from another user and account rewrites the
    // numbers, which are on the list, and nothing that says whose row or
    // what kind of row it is.
    let taken = write(balance(home.carol, "acc-9", "2026-03-05T21:00:00Z", 5.0), Some(BALANCE_SNAPSHOT_COLUMNS)).await?.one()?;
    assert_eq!(
        (taken.get::<Uuid>("user_id")?, taken.get::<String>("account_id")?, taken.get::<Decimal>("balances_current")?),
        (home.alice, "acc-1".to_string(), dec("5"))
    );

    // A soft delete hides it from live reads; the next sync's write
    // brings it back, since `is_deleted` is on the list and the row
    // always says `false`.
    assert!(soft_delete(&db, "snapshots", "acc-1-20260305", Scope::Group(home.bob)).await?);
    assert!(!soft_delete(&db, "snapshots", "acc-1-20260305", Scope::Group(home.carol)).await?);
    assert!(db.query(find(Query::table("snapshots"), &Filters::new(Scope::Group(home.alice)))).await?.is_empty());
    let revived = write(balance(home.alice, "acc-1", "2026-03-05T22:00:00Z", 1300.0), Some(BALANCE_SNAPSHOT_COLUMNS)).await?.one()?;
    assert_eq!((revived.get::<Option<bool>>("is_deleted")?, revived.get::<Decimal>("balances_current")?), (Some(false), dec("1300")));

    // A security's price belongs to nobody, and its write does not name
    // `is_deleted`: a deleted price row stays deleted.
    let price = |close: f64| {
        data! {
            "snapshot_id": "sec-1-20260305", "snapshot_date": "2026-03-05T00:00:00Z", "snapshot_type": "security", "security_id": "sec-1",
            "close_price": close,
        }
    };
    let priced = write(price(250.1234567), None).await?.one()?;
    assert_eq!(
        (priced.get::<Option<Uuid>>("user_id")?, priced.get::<Option<bool>>("is_deleted")?, priced.get::<Decimal>("close_price")?.to_string()),
        (None, Some(false), "250.123457".to_string())
    );
    assert!(soft_delete(&db, "snapshots", "sec-1-20260305", Scope::Everyone).await?);
    let repriced = write(price(251.0), None).await?.one()?;
    assert_eq!((repriced.get::<Decimal>("close_price")?, repriced.get::<Option<bool>>("is_deleted")?), (dec("251"), Some(true)));
    // No group's scope matches a row without an owner: these are read
    // without one.
    assert!(find_one(&db, Query::table("snapshots").key(["sec-1-20260305"]), &Filters::new(Scope::Group(home.alice)).with_tombstones()).await?.is_none());
    assert!(find_one(&db, Query::table("snapshots").key(["sec-1-20260305"]), &Filters::new(Scope::Everyone).with_tombstones()).await?.is_some());

    // A holding's snapshot, with whichever amounts the sync has. A later
    // write that has only the price leaves the others.
    let holding = "holding-acc-1-sec-1-20260305";
    let base = |account: &'static str, at: &'static str| {
        data! {
            "snapshot_id": holding, "user_id": home.alice, "snapshot_date": at, "snapshot_type": "holding", "is_deleted": false,
            "holding_account_id": account, "holding_security_id": "sec-1",
        }
    };
    let mut first = base("acc-1", "2026-03-05T08:00:00Z");
    first.extend(data! {"quantity": 10, "institution_value": 2501.23});
    write(first, Some(HOLDING_SNAPSHOT_COLUMNS)).await?;
    let mut second = base("acc-other", "2026-03-05T09:00:00Z");
    second.extend(data! {"institution_price": 250.123});
    let row = write(second, Some(HOLDING_SNAPSHOT_COLUMNS)).await?.one()?;
    assert_eq!(
        (row.get::<Decimal>("quantity")?, row.get::<Decimal>("institution_value")?, row.get::<Decimal>("institution_price")?.to_string()),
        (dec("10"), dec("2501.23"), "250.123000".to_string())
    );
    assert_eq!((row.get::<String>("holding_account_id")?, row.get::<Timestamp>("snapshot_date")?), ("acc-1".to_string(), ts("2026-03-05T09:00:00Z")));
    // A holding row keeps its account in `holding_account_id`; the plain
    // account and security columns stay NULL.
    assert!(row.is_null("account_id")? && row.is_null("security_id")? && row.is_null("cost_basis")?);

    // One holding snapshot by id and kind, within the group.
    let holding_kind = |viewer: Uuid| Filters::new(Scope::Group(viewer)).eq("snapshot_type", "holding");
    assert!(find_one(&db, Query::table("snapshots").key([holding]), &holding_kind(home.bob)).await?.is_some());
    assert!(find_one(&db, Query::table("snapshots").key([holding]), &holding_kind(home.carol)).await?.is_none());
    assert!(find_one(&db, Query::table("snapshots").key(["acc-1-20260305"]), &holding_kind(home.bob)).await?.is_none());
    // The fields a request named, patched within the group.
    assert_eq!(update_status(&update(&db, "snapshots", holding, data! {"quantity": 12.5, "cost_basis": 2000}, Scope::Group(home.bob), vec![]).await?), 200);
    assert_eq!(update_status(&update(&db, "snapshots", holding, data! {"quantity": 0}, Scope::Group(home.carol), vec![]).await?), 304);
    let patched = stored(&db, "snapshots", holding).await?;
    assert_eq!(
        (patched.get::<Decimal>("quantity")?, patched.get::<Decimal>("cost_basis")?, patched.get::<Decimal>("institution_value")?),
        (dec("12.5"), dec("2000"), dec("2501.23"))
    );

    // An account's snapshots go with it, through whichever column holds the
    // account for their kind; others are deleted by id.
    for (id, owner, kind, account) in [
        ("acc-2-20260305", home.alice, "account_balance", "acc-2"),
        ("holding-acc-2-sec-1-20260305", home.bob, "holding", "acc-2"),
        ("acc-9-20260305", home.carol, "account_balance", "acc-9"),
        ("holding-acc-9-sec-1-20260305", home.carol, "holding", "acc-9"),
    ] {
        let column = if kind == "holding" { "holding_account_id" } else { "account_id" };
        let mut row = data! {"snapshot_id": id, "user_id": owner, "snapshot_date": "2026-03-05T08:00:00Z", "snapshot_type": kind};
        row.push((column, account.into()));
        write(row, None).await?;
    }
    assert_eq!(bulk_soft_delete_by_column(&db, "snapshots", "account_id", "acc-1", Scope::Group(home.alice)).await?, 1);
    assert_eq!(bulk_soft_delete_by_column(&db, "snapshots", "holding_account_id", vec!["acc-1", "acc-2", "acc-9"], Scope::Group(home.alice)).await?, 2);
    assert_eq!(
        bulk_soft_delete(&db, "snapshots", vec!["acc-2-20260305".into(), "acc-9-20260305".into(), "sec-1-20260305".into()], Scope::Group(home.alice)).await?,
        1
    );
    let live_ids = texts_of(&db.query(find(Query::table("snapshots"), &Filters::new(Scope::Everyone))).await?, "snapshot_id");
    assert_eq!(live_ids, ["acc-9-20260305", "holding-acc-9-sec-1-20260305"]);
    assert_eq!(update_status(&update(&db, "snapshots", holding, data! {"quantity": 1}, Scope::Group(home.alice), vec![]).await?), 304);
    Ok(())
}

// R-Q9, R-Q11, R-Q4, R-Q8, R-Q6, R-T9, R-S4: the latest balance of each account, the latest price of a security,
// the prices written lately, and three chained keyset streams.
#[tokio::test(flavor = "multi_thread")]
async fn the_latest_snapshot_is_read_per_account_and_per_security() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    for (id, owner) in [("acc-a", home.alice), ("acc-b", home.bob), ("acc-c", home.carol), ("acc-new", home.alice)] {
        account(&db, owner, id).await?;
    }
    // (account, owner, day, balance)
    for (account, owner, day, current) in [
        ("acc-a", home.alice, "01", 100),
        ("acc-a", home.alice, "03", 120),
        ("acc-a", home.alice, "05", 90),
        ("acc-b", home.bob, "02", 50),
        ("acc-b", home.bob, "04", 60),
        ("acc-c", home.carol, "05", 7),
    ] {
        let row = data! {
            "snapshot_id": format!("{account}-202603{day}"), "user_id": owner, "snapshot_date": format!("2026-03-{day}T06:00:00Z"),
            "snapshot_type": "account_balance", "account_id": account, "balances_current": current,
        };
        upsert(&db, "snapshots", row, None, None).await?;
    }
    // (security, day)
    for (security, day) in [("sec-1", "02"), ("sec-1", "04"), ("sec-1", "06"), ("sec-2", "01"), ("sec-3", "01")] {
        let row = data! {
            "snapshot_id": format!("{security}-202603{day}"), "snapshot_date": format!("2026-03-{day}T00:00:00Z"), "snapshot_type": "security",
            "security_id": security, "close_price": 10,
        };
        upsert(&db, "snapshots", row, None, None).await?;
    }
    // (id, owner, account, security, day)
    for (id, owner, account, security, day) in [
        ("hold-a-sec1-01", home.alice, "acc-a", "sec-1", "01"),
        ("hold-a-sec1-02", home.alice, "acc-a", "sec-1", "02"),
        ("hold-a-sec2-01", home.alice, "acc-a", "sec-2", "01"),
        ("hold-a-sec3-01", home.alice, "acc-a", "sec-3", "01"),
        ("hold-b-sec1-01", home.bob, "acc-b", "sec-1", "01"),
    ] {
        let row = data! {
            "snapshot_id": id, "user_id": owner, "snapshot_date": format!("2026-03-{day}T00:00:00Z"), "snapshot_type": "holding",
            "holding_account_id": account, "holding_security_id": security, "quantity": 1,
        };
        upsert(&db, "snapshots", row, None, None).await?;
    }
    for deleted in ["acc-a-20260305", "sec-1-20260306", "sec-3-20260301"] {
        soft_delete(&db, "snapshots", deleted, Scope::Everyone).await?;
    }

    // The latest live balance of every account of the group, in one
    // statement: per account, the index over `(account_id, snapshot_date)`
    // is read from its far end and the first live row taken. An account
    // without a snapshot still has its row.
    let latest = Query::table("accounts")
        .alias("a")
        .filter(col("a.user_id").in_list(sharing_user_ids(home.alice)))
        .filter(live_in("a"))
        .left_join(
            Query::table("snapshots")
                .alias("s")
                .index("idx_snapshots_account_id_snapshot_date", KeyRange::eq([col("a.account_id")]))
                .reverse()
                .filter(live_in("s").and(col("s.snapshot_type").eq("account_balance")))
                .limit(1),
        )
        .select("account_id", col("a.account_id"))
        .select("snapshot_id", col("s.snapshot_id"))
        .select("balance", col("s.balances_current"));
    let rows: Vec<(String, Option<String>, Option<Decimal>)> = db.query(latest).await?.map()?;
    assert_eq!(
        rows,
        [
            ("acc-a".to_string(), Some("acc-a-20260303".to_string()), Some(dec("120"))),
            ("acc-b".to_string(), Some("acc-b-20260304".to_string()), Some(dec("60"))),
            ("acc-new".to_string(), None, None)
        ]
    );
    // The same as an aggregate over the group's balance rows.
    let members = sharing_group(&db, home.alice).await?;
    let per_account = db
        .query(
            Query::table("snapshots")
                .index_ranges("idx_snapshots_user_id_snapshot_date", members.iter().map(|member| KeyRange::eq([*member])))
                .filter(col("snapshot_type").eq("account_balance"))
                .filter(live())
                .group_by([col("account_id")])
                .select("account_id", col("account_id"))
                .select("latest", max(col("snapshot_date")))
                .select("n", count())
                .order_by(col("account_id")),
        )
        .await?;
    assert_eq!(texts_of(&per_account, "account_id"), ["acc-a", "acc-b"]);
    assert_eq!(column::<Timestamp>(&per_account, "latest"), [ts("2026-03-03T06:00:00Z"), ts("2026-03-04T06:00:00Z")]);
    assert_eq!(column::<i64>(&per_account, "n"), [2, 2]);

    // The day of a security's latest live price. `MAX` over no rows is one
    // row holding NULL; the caller formats the instant in UTC.
    let latest_price_day = |security: &'static str| {
        let db = &db;
        async move {
            let row = db
                .query_one(
                    Query::table("snapshots")
                        .index("idx_snapshots_security_id_snapshot_date", KeyRange::eq([security]))
                        .filter(col("snapshot_type").eq("security"))
                        .filter(live())
                        .select("latest", max(col("snapshot_date"))),
                )
                .await?;
            Ok::<_, DbError>(row.get::<Option<Timestamp>>("latest")?.map(|at| at.to_rfc3339()[..10].to_string()))
        }
    };
    assert_eq!(latest_price_day("sec-1").await?.as_deref(), Some("2026-03-04"));
    assert_eq!(latest_price_day("sec-2").await?.as_deref(), Some("2026-03-01"));
    assert_eq!(latest_price_day("sec-3").await?, None);
    assert_eq!(latest_price_day("sec-9").await?, None);
    // The index's far end gives the same row without an aggregate.
    let newest = db
        .query_opt(Query::table("snapshots").index("idx_snapshots_security_id_snapshot_date", KeyRange::eq(["sec-1"])).reverse().filter(live()).limit(1))
        .await?;
    assert_eq!(newest.expect("a live price").get::<String>("snapshot_id")?, "sec-1-20260304");

    // Which of some securities had a price written in the last 22
    // hours, by the server's clock. Deleted rows count; a security
    // written twice is listed once.
    let now = db.now().await?;
    backdate(&db, "snapshots", "sec-2-20260301", now.plus_secs(-23 * 3600)).await?;
    backdate(&db, "snapshots", "sec-1-20260302", now.plus_secs(-21 * 3600)).await?;
    let asked = ["sec-1", "sec-2", "sec-3", "sec-9"];
    let recent = Query::table("snapshots")
        .index_ranges("idx_snapshots_security_id_snapshot_date", asked.map(|security| KeyRange::eq([security])))
        .filter(col("snapshot_type").eq("security").and(col("updated").gt(expr::now() - hours(22))))
        .select("security_id", col("security_id"))
        .distinct();
    assert_eq!(texts_of(&db.query(recent).await?, "security_id"), ["sec-1", "sec-3"]);

    // The holding snapshots of one account for some securities, by time and
    // then by id.
    let of_account = Query::table("snapshots")
        .index("idx_snapshots_holding_account_id", KeyRange::eq(["acc-a"]))
        .filter(col("user_id").in_list(sharing_user_ids(home.bob)))
        .filter(col("snapshot_type").eq("holding").and(col("holding_security_id").in_list(vec!["sec-1", "sec-2"])))
        .filter(live())
        .order_by(col("snapshot_date"))
        .order_by(col("snapshot_id"));
    assert_eq!(texts_of(&db.query(of_account).await?, "snapshot_id"), ["hold-a-sec1-01", "hold-a-sec2-01", "hold-a-sec1-02"]);

    // A full read of the snapshots chains three streams, each paged by id:
    // the group's balances, the group's holding rows for its accounts, and
    // the prices, which have no owner and are read without a scope.
    let group = Filters::new(Scope::Group(home.alice)).with_tombstones();
    let stream = |filters: Filters| {
        let db = &db;
        async move { find_in_batches(db, "snapshots", &filters, 2).await?.concat().iter().map(|row| row.get("snapshot_id")).collect::<DbResult<Vec<String>>>() }
    };
    assert_eq!(
        stream(group.clone().eq("snapshot_type", "account_balance")).await?,
        ["acc-a-20260301", "acc-a-20260303", "acc-a-20260305", "acc-b-20260302", "acc-b-20260304"]
    );
    assert_eq!(
        stream(group.clone().eq("snapshot_type", "holding").any_of("holding_account_id", ["acc-a", "acc-b"]).eq("holding_security_id", "sec-1")).await?,
        ["hold-a-sec1-01", "hold-a-sec1-02", "hold-b-sec1-01"]
    );
    assert_eq!(stream(Filters::new(Scope::Everyone).eq("snapshot_type", "security")).await?, ["sec-1-20260302", "sec-1-20260304", "sec-2-20260301"]);
    assert_eq!(
        stream(
            Filters::new(Scope::Everyone)
                .eq("snapshot_type", "security")
                .eq("security_id", "sec-2")
                .since("updated", now.plus_secs(-24 * 3600))
                .until("updated", now.plus_secs(-22 * 3600))
        )
        .await?,
        ["sec-2-20260301"]
    );
    // The builder could not say "mine or nobody's", hence three reads.
    // As an expression it is one condition.
    let mine_or_global = Query::table("snapshots").filter(col("user_id").in_list(sharing_user_ids(home.alice)).or(col("user_id").is_null())).filter(live());
    assert_eq!(db.query(mine_or_global).await?.len(), 4 + 5 + 3);
    // The price stream can also page through `(snapshot_type,
    // snapshot_id)`, which skips the other kinds instead of filtering.
    let page = |after: Bound| {
        Query::table("snapshots")
            .index("idx_snapshots_snapshot_type_snapshot_id", KeyRange::within(["security"], after, Bound::Unbounded))
            .filter(live())
            .limit(2)
    };
    let first = texts_of(&db.query(page(Bound::Unbounded)).await?, "snapshot_id");
    assert_eq!(first, ["sec-1-20260302", "sec-1-20260304"]);
    assert_eq!(texts_of(&db.query(page(Bound::excluded([first[1].as_str()]))).await?, "snapshot_id"), ["sec-2-20260301"]);
    Ok(())
}

// R-Q9, R-Q11, R-Q1, R-Q6, R-S4, R-T6: `SELECT DISTINCT s.*` over a join, which compares whole rows, JSON included.
#[tokio::test(flavor = "multi_thread")]
async fn securities_behind_a_groups_holdings_are_distinct_whole_rows() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    for (id, ticker, name) in [
        ("sec-1", Some("VTI"), "Total Market"),
        ("sec-2", Some("BND"), "Total Bond"),
        ("sec-3", None, "Cash"),
        ("sec-4", Some("VXUS"), "International"),
        ("sec-5", Some("VTI"), "Total Market (new id)"),
    ] {
        upsert(
            &db,
            "securities",
            data! {
                "security_id": id, "name": name, "ticker_symbol": ticker, "type": "etf",
                "raw": json!({"sector": "n/a", "is_cash_equivalent": id == "sec-3", "ids": [id]}),
            },
            None,
            None,
        )
        .await?;
    }
    // (holding, owner, account, security)
    for (id, owner, account, security) in [
        ("h1", home.alice, "acc-a", "sec-1"),
        ("h2", home.alice, "acc-a2", "sec-1"),
        ("h3", home.alice, "acc-a", "sec-2"),
        ("h4", home.bob, "acc-b", "sec-1"),
        ("h5", home.carol, "acc-c", "sec-3"),
        ("h6", home.bob, "acc-b", "sec-3"),
    ] {
        upsert(
            &db,
            "holdings",
            data! {"user_id": owner, "holding_id": id, "account_id": account, "security_id": security, "quantity": 1},
            Some(HOLDING_COLUMNS),
            None,
        )
        .await?;
    }
    // Holdings deleted by id, within the group.
    assert_eq!(bulk_soft_delete(&db, "holdings", vec!["h3".into(), "h5".into(), "h6".into()], Scope::Group(home.alice)).await?, 2);

    // The securities any holding of the group points at, each once, as
    // whole rows. Deleted holdings count on purpose: their history still
    // needs the security.
    let held_by = |viewer: Uuid| {
        Query::table("securities").alias("s").join(
            Query::table("holdings")
                .alias("h")
                .index("idx_holdings_security_id", KeyRange::eq([col("s.security_id")]))
                .filter(col("h.user_id").in_list(sharing_user_ids(viewer))),
        )
    };
    let held = db.query(held_by(home.alice).distinct()).await?;
    assert_eq!(texts_of(&held, "security_id"), ["sec-1", "sec-2", "sec-3"]);
    assert_eq!(held.columns(), all_columns("securities"));
    assert_eq!(held.first().unwrap().get::<Json>("raw")?, json!({"ids": ["sec-1"], "sector": "n/a", "is_cash_equivalent": false}));
    // Without `DISTINCT` a security comes once per holding.
    assert_eq!(texts_of(&db.query(held_by(home.alice)).await?, "security_id"), ["sec-1", "sec-1", "sec-1", "sec-2", "sec-3"]);
    assert_eq!(texts_of(&db.query(held_by(home.carol).distinct()).await?, "security_id"), ["sec-3"]);

    // One by id, in a loop over the ids a caller has.
    let mut names = Vec::new();
    for id in ["sec-2", "sec-9", "sec-4"] {
        if let Some(row) = find_one(&db, Query::table("securities").key([id]), &Filters::new(Scope::Everyone)).await? {
            names.push(row.get::<String>("name")?);
        }
    }
    assert_eq!(names, ["Total Bond", "International"]);

    // Search by exact fields, newest first; the caller takes the first row,
    // so the order decides between two rows of one ticker. The index keeps
    // each ticker's rows by `updated`, descending.
    let now = db.now().await?;
    backdate(&db, "securities", "sec-1", now.plus_secs(-7_200)).await?;
    backdate(&db, "securities", "sec-5", now.plus_secs(-3_600)).await?;
    let by_ticker = |ticker: &'static str| Query::table("securities").index("idx_securities_ticker_symbol_updated", KeyRange::eq([ticker]));
    assert_eq!(texts_of(&db.query(by_ticker("VTI")).await?, "security_id"), ["sec-5", "sec-1"]);
    assert_eq!(
        find_one(&db, by_ticker("VTI"), &Filters::new(Scope::Everyone).eq("name", "Total Market"))
            .await?
            .expect("the older row")
            .get::<String>("security_id")?,
        "sec-1"
    );
    upsert(&db, "securities", data! {"security_id": "sec-1", "close_price": 250}, None, None).await?;
    assert_eq!(find_one(&db, by_ticker("VTI"), &Filters::new(Scope::Everyone)).await?.expect("the row just written").get::<String>("security_id")?, "sec-1");
    // The builder's form: any of the filters, sorted by the caller.
    let searched =
        db.query(find(Query::table("securities"), &Filters::new(Scope::Everyone).eq("ticker_symbol", "VTI")).order_by(col("updated").desc())).await?;
    assert_eq!(texts_of(&searched, "security_id"), ["sec-1", "sec-5"]);
    assert_eq!(db.query(find(Query::table("securities"), &Filters::new(Scope::Everyone)).order_by(col("updated").desc())).await?.len(), 5);

    // The group's live holdings, all or of some accounts.
    let group = Filters::new(Scope::Group(home.alice));
    assert_eq!(texts_of(&db.query(find(Query::table("holdings"), &group)).await?, "holding_id"), ["h1", "h2", "h4"]);
    let of_accounts = Query::table("holdings").index_ranges("idx_holdings_account_id", ["acc-a2", "acc-b", "acc-c"].map(|account| KeyRange::eq([account])));
    assert_eq!(texts_of(&db.query(find(of_accounts, &group)).await?, "holding_id"), ["h2", "h4"]);
    assert_eq!(
        texts_of(&db.query(find(Query::table("holdings"), &group.clone().any_of("account_id", ["acc-a2", "acc-b", "acc-c"]))).await?, "holding_id"),
        ["h2", "h4"]
    );
    // Every security anyone still holds, for the price refresh.
    let still_held = Query::table("holdings")
        .index("idx_holdings_security_id", KeyRange::all())
        .filter(live())
        .filter(col("security_id").is_not_null())
        .select("security_id", col("security_id"))
        .distinct();
    assert_eq!(texts_of(&db.query(still_held).await?, "security_id"), ["sec-1", "sec-3"]);
    Ok(())
}

/// A plan-statement import: the statement's rows in chunks of 1000 that
/// never overwrite, the fund mappings, the old holding snapshots read and
/// soft-deleted when replacing, the holdings. Any failure fails the import.
async fn import_plan_statement(
    db: &Client,
    owner: Uuid,
    account: &'static str,
    rows: Vec<Data>,
    mappings: Json,
    holdings: Vec<Data>,
) -> DbResult<(usize, u64)> {
    db.exclusive(|tx| async move {
        let mut inserted = 0;
        for chunk in rows.chunks(1000) {
            inserted += upsert_many(&tx, "investment_transactions", chunk.to_vec(), Some(&[])).await?.len();
        }
        update(&tx, "accounts", account, data! {"plan_fund_mappings": mappings}, Scope::Owner(owner), vec![]).await?;
        let old = tx
            .query(
                Query::table("snapshots")
                    .index("idx_snapshots_holding_account_id", KeyRange::eq([account]))
                    .filter(col("user_id").in_list(sharing_user_ids(owner)))
                    .filter(col("snapshot_type").eq("holding"))
                    .filter(live())
                    .order_by(col("snapshot_date"))
                    .order_by(col("snapshot_id"))
                    .columns(["snapshot_id"]),
            )
            .await?;
        let replaced =
            bulk_soft_delete(&tx, "snapshots", old.iter().map(|row| row.get::<Value>("snapshot_id")).collect::<DbResult<Vec<Value>>>()?, Scope::Group(owner))
                .await?;
        for holding in holdings {
            upsert(&tx, "holdings", holding, Some(HOLDING_COLUMNS), None).await?;
        }
        Ok((inserted, replaced))
    })
    .await
}

// R-W2, R-X1, R-X7, R-W5, R-T6, R-W11: a transaction of thousands of rows over many statements, atomic.
#[tokio::test(flavor = "multi_thread")]
async fn a_plan_statement_import_lands_whole_or_not_at_all() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    account(&db, home.alice, "acc-401k").await?;
    for day in ["01", "02"] {
        upsert(
            &db,
            "snapshots",
            data! {
                "snapshot_id": format!("holding-acc-401k-sec-1-202602{day}"), "user_id": home.alice, "snapshot_date": format!("2026-02-{day}T00:00:00Z"),
                "snapshot_type": "holding", "holding_account_id": "acc-401k", "holding_security_id": "sec-1", "quantity": 1,
            },
            None,
            None,
        )
        .await?;
    }
    // The ids are derived from the statement's lines, so importing the
    // same statement twice proposes the same rows.
    let lines = |from: usize, to: usize, amount: f64| {
        (from..to)
            .map(|i| {
                data! {
                    "user_id": home.alice, "investment_transaction_id": format!("plan-{i:032x}"), "account_id": "acc-401k",
                    "security_id": if i % 2 == 0 { "sec-1" } else { "sec-2" }, "date": "2026-03-05", "name": "Contribution", "amount": amount,
                    "quantity": 0.123456, "price": 99.95, "type": "buy", "source": "plan",
                }
            })
            .collect::<Vec<Data>>()
    };
    let holdings = |quantity: f64| {
        vec![
            data! {
                "user_id": home.alice, "holding_id": "acc-401k-sec-1", "account_id": "acc-401k", "security_id": "sec-1", "quantity": quantity,
                "institution_value": 5000.5,
            },
            data! {"user_id": home.alice, "holding_id": "acc-401k-sec-2", "account_id": "acc-401k", "security_id": "sec-2", "quantity": 1},
        ]
    };
    let mappings = json!({"Target 2050": {"security_id": "sec-1", "restated": false}, "Bond": {"security_id": "sec-2", "restated": false}});
    let stored_lines = || db.query(Query::table("investment_transactions").index("idx_investment_transactions_account_id_date", KeyRange::eq(["acc-401k"])));

    // 2,300 lines go in as three statements of one transaction.
    assert_eq!(import_plan_statement(&db, home.alice, "acc-401k", lines(0, 2300, 12.34), mappings.clone(), holdings(40.5)).await?, (2300, 2));
    let all = stored_lines().await?;
    assert_eq!(all.len(), 2300);
    // They, the account and the holdings all carry the transaction's time.
    let mut stamps: BTreeSet<Timestamp> = column::<Timestamp>(&all, "updated").into_iter().collect();
    stamps.insert(stored(&db, "accounts", "acc-401k").await?.get("updated")?);
    stamps.insert(stored(&db, "holdings", "acc-401k-sec-1").await?.get("updated")?);
    assert_eq!(stamps.len(), 1);
    assert_eq!(
        stored(&db, "accounts", "acc-401k").await?.get::<Json>("plan_fund_mappings")?.to_string(),
        r#"{"Bond":{"restated":false,"security_id":"sec-2"},"Target 2050":{"restated":false,"security_id":"sec-1"}}"#
    );
    assert!(db.query(find(Query::table("snapshots"), &Filters::new(Scope::Owner(home.alice)))).await?.is_empty());
    let sum = db.query_one(Query::table("investment_transactions").select("total", sum(col("amount"))).select("units", sum(col("quantity")))).await?;
    assert_eq!((sum.get::<Decimal>("total")?.to_string(), sum.get::<Decimal>("units")?.to_string()), ("28382.00".to_string(), "283.948800".to_string()));

    // The same statement again, with another amount on every line and
    // 200 lines more: only the new lines are inserted, and no line that
    // was there is overwritten.
    assert_eq!(import_plan_statement(&db, home.alice, "acc-401k", lines(0, 2500, 99.0), mappings.clone(), holdings(41.0)).await?, (200, 0));
    let all = stored_lines().await?;
    assert_eq!((all.len(), all.iter().filter(|row| row.get::<Decimal>("amount").unwrap() == dec("99")).count()), (2500, 200));
    assert_eq!(stored(&db, "holdings", "acc-401k-sec-1").await?.get::<Decimal>("quantity")?, dec("41"));

    // An import whose last holding cannot be stored fails as a whole:
    // not one of its 1,500 new lines stays, the mappings are the old
    // ones, and so is the first holding it had already rewritten.
    let failed = import_plan_statement(
        &db,
        home.alice,
        "acc-401k",
        lines(5000, 6500, 1.0),
        json!({}),
        vec![
            holdings(42.0).remove(0),
            data! {"user_id": home.alice, "holding_id": "acc-401k-sec-2", "account_id": "acc-401k", "security_id": "sec-2", "quantity": 1e12},
        ],
    )
    .await;
    assert_eq!(code(failed), ErrorCode::OutOfRange);
    assert_eq!(stored_lines().await?.len(), 2500);
    assert_eq!(stored(&db, "accounts", "acc-401k").await?.get::<Json>("plan_fund_mappings")?, mappings);
    assert_eq!(stored(&db, "holdings", "acc-401k-sec-1").await?.get::<Decimal>("quantity")?, dec("41"));
    Ok(())
}

/// Record that a category was rejected for a transaction, under the
/// transaction's owner. The row to insert is selected from the transaction
/// itself, so nothing is proposed for a transaction outside the viewer's
/// group. An existing rejection is stamped again, unless it is recorded
/// under someone else.
fn record_rejection(viewer: Uuid, transaction: &str, category: Uuid) -> Insert {
    let proposed = Query::table("transactions")
        .alias("t")
        .key([transaction])
        .filter(col("t.user_id").in_list(sharing_user_ids(viewer)))
        .select("transaction_id", col("t.transaction_id"))
        .select("user_id", col("t.user_id"))
        .select("category_id", val(category));
    Insert::into("rejected_categories")
        .columns(["transaction_id", "user_id", "category_id"])
        .from_query(proposed)
        .on_conflict_update(PRIMARY)
        .update("rejected_at", now())
        .update_where(col("user_id").eq(excluded("user_id")))
        .returning(["transaction_id", "user_id", "category_id", "rejected_at"])
}

// R-W3, R-W4, R-W5, R-W10, R-S2, R-X1, R-Q4: `INSERT .. SELECT .. ON CONFLICT` on a composite primary key, with a
// conditional update; rejections are listed, moved to another transaction and cleared.
#[tokio::test(flavor = "multi_thread")]
async fn rejections_are_recorded_under_the_transactions_owner_and_move_with_it() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let budget: Uuid = insert(&db, "budgets", data! {"user_id": home.alice}, &[], false).await?.one()?.get("budget_id")?;
    let section: Uuid = insert(&db, "sections", data! {"user_id": home.alice, "budget_id": budget}, &[], false).await?.one()?.get("section_id")?;
    let mut categories = Vec::new();
    for name in ["food", "fun", "fuel"] {
        categories.push(
            insert(&db, "categories", data! {"user_id": home.alice, "section_id": section, "name": name}, &[], false)
                .await?
                .one()?
                .get::<Uuid>("category_id")?,
        );
    }
    let (food, fun, fuel) = (categories[0], categories[1], categories[2]);
    for (id, owner) in [("pending-1", home.alice), ("posted-1", home.alice), ("t-bob", home.bob), ("t-carol", home.carol)] {
        txn(&db, owner, id, "acc-1", 10, "2026-03-05").await?;
    }
    let long_ago = ts("2001-01-01T00:00:00Z");
    let reject = |viewer: Uuid, transaction: &'static str, category: Uuid| db.insert(record_rejection(viewer, transaction, category));

    // Bob rejects a suggestion on Alice's transaction: the row is hers.
    let recorded = reject(home.bob, "pending-1", food).await?;
    assert_eq!((recorded.affected(), recorded.columns().len()), (1, 4));
    let row = recorded.one()?;
    assert_eq!(
        (row.get::<String>("transaction_id")?, row.get::<Uuid>("user_id")?, row.get::<Uuid>("category_id")?),
        ("pending-1".to_string(), home.alice, food)
    );
    // Rejecting it again keeps the one row and stamps it with now.
    db.update(Update::table("rejected_categories").key([val("pending-1"), val(food)]).set("rejected_at", long_ago)).await?;
    let again = reject(home.alice, "pending-1", food).await?.one()?;
    assert!(again.get::<Timestamp>("rejected_at")? > long_ago);
    assert_eq!(db.query(Query::table("rejected_categories")).await?.len(), 1);

    // No row comes back, and none is written, for a transaction outside
    // the group or one that does not exist: nothing was selected.
    for (viewer, transaction) in [(home.alice, "t-carol"), (home.carol, "pending-1"), (home.alice, "t-none")] {
        let nothing = reject(viewer, transaction, food).await?;
        assert_eq!((nothing.affected(), nothing.len()), (0, 0), "{transaction}");
    }
    // Nor when the rejection exists under another user: the condition on
    // the conflict declines, and the row keeps its stamp.
    db.insert(
        Insert::into("rejected_categories").set("transaction_id", "t-bob").set("user_id", home.carol).set("category_id", food).set("rejected_at", long_ago),
    )
    .await?;
    let declined = reject(home.alice, "t-bob", food).await?;
    assert_eq!((declined.affected(), declined.len()), (0, 0));
    let kept = db.query_one(Query::table("rejected_categories").key([val("t-bob"), val(food)])).await?;
    assert_eq!((kept.get::<Uuid>("user_id")?, kept.get::<Timestamp>("rejected_at")?), (home.carol, long_ago));
    // A category that does not exist fails the statement; the caller
    // logs it and moves on.
    assert_eq!(code(reject(home.alice, "pending-1", uuid(404)).await), ErrorCode::ForeignKeyViolation);

    // The rejections of some transactions, by transaction and then
    // newest first.
    reject(home.alice, "pending-1", fun).await?;
    reject(home.alice, "posted-1", fun).await?;
    db.update(Update::table("rejected_categories").key([val("pending-1"), val(fun)]).set("rejected_at", ts("2001-06-01T00:00:00Z"))).await?;
    let listed = db
        .query(
            Query::table("rejected_categories")
                .keys([["posted-1"], ["pending-1"], ["t-bob"]])
                .filter(col("user_id").eq(home.alice))
                .order_by(col("transaction_id"))
                .order_by(col("rejected_at").desc()),
        )
        .await?;
    assert_eq!(texts_of(&listed, "transaction_id"), ["pending-1", "pending-1", "posted-1"]);
    assert_eq!(column::<Uuid>(&listed, "category_id"), [food, fun, fun]);

    // The pending transaction was replaced by the posted one. Its
    // rejections are copied over, except the one the posted transaction
    // already has, and the originals deleted, in one transaction. The count
    // of rows copied is the function's answer.
    reject(home.alice, "pending-1", fuel).await?;
    db.update(Update::table("rejected_categories").key([val("pending-1"), val(fuel)]).set("rejected_at", long_ago)).await?;
    let moved = db
        .transaction(|tx| async move {
            let copies = Query::table("rejected_categories")
                .range(KeyRange::eq(["pending-1"]))
                .select("transaction_id", val("posted-1"))
                .select("user_id", col("user_id"))
                .select("category_id", col("category_id"))
                .select("rejected_at", col("rejected_at"));
            let copied = tx
                .insert(
                    Insert::into("rejected_categories")
                        .columns(["transaction_id", "user_id", "category_id", "rejected_at"])
                        .from_query(copies)
                        .on_conflict_do_nothing_on(PRIMARY),
                )
                .await?
                .affected();
            let deleted = tx.delete(Delete::rows(Query::table("rejected_categories").range(KeyRange::eq(["pending-1"])))).await?.affected();
            Ok::<_, DbError>((copied, deleted))
        })
        .await?;
    assert_eq!(moved, (2, 3));
    let now_on_posted = db.query(Query::table("rejected_categories").range(KeyRange::eq(["posted-1"])).order_by(col("rejected_at"))).await?;
    assert_eq!(now_on_posted.len(), 3);
    // A copy keeps the time of the original rejection.
    assert_eq!(now_on_posted.first().unwrap().get::<Timestamp>("rejected_at")?, long_ago);
    assert!(db.query(Query::table("rejected_categories").range(KeyRange::eq(["pending-1"]))).await?.is_empty());

    // A rejection is cleared by the three of its columns, within the group;
    // the row count is the answer.
    let clear = |viewer: Uuid, transaction: &'static str, category: Uuid| {
        db.delete(Delete::from("rejected_categories").key([val(transaction), val(category)]).filter(col("user_id").in_list(sharing_user_ids(viewer))))
    };
    assert_eq!(clear(home.carol, "posted-1", food).await?.affected(), 0);
    assert_eq!(clear(home.bob, "posted-1", food).await?.affected(), 1);
    assert_eq!(clear(home.bob, "posted-1", food).await?.affected(), 0);
    // A soft-deleted category keeps its rejections.
    assert!(soft_delete(&db, "categories", fun, Scope::Group(home.alice)).await?);
    assert_eq!(db.query(Query::table("rejected_categories").range(KeyRange::eq(["posted-1"]))).await?.len(), 2);
    Ok(())
}

/// Deleting an item: the item's accounts are read first, on the pool; then
/// one transaction soft-deletes everything that hangs off those accounts,
/// the pairs that name their transactions (after the transactions, which
/// the order of the cascade depends on), the accounts and the item. The
/// item's row count is the caller's answer.
async fn delete_item(db: &Client, viewer: Uuid, item: &'static str) -> DbResult<u64> {
    let scope = Scope::Group(viewer);
    let accounts: Vec<String> = db
        .query(find(Query::table("accounts").index("idx_accounts_item_id", KeyRange::eq([item])), &Filters::new(scope)).columns(["account_id"]))
        .await?
        .column()?;
    db.exclusive(|tx| async move {
        let ids = || accounts.iter().map(Value::from).collect::<Vec<Value>>();
        for (table, column) in [
            ("transactions", "account_id"),
            ("investment_transactions", "account_id"),
            ("split_transactions", "account_id"),
            ("snapshots", "account_id"),
            ("snapshots", "holding_account_id"),
            ("holdings", "account_id"),
        ] {
            bulk_soft_delete_by_column(&tx, table, column, ids(), scope).await?;
        }
        for side in ["transaction_id_a", "transaction_id_b"] {
            bulk_soft_delete_by_related_column(&tx, "transaction_pairs", side, ("transactions", "transaction_id", "account_id"), ids(), scope).await?;
        }
        bulk_soft_delete(&tx, "accounts", ids(), scope).await?;
        bulk_soft_delete(&tx, "items", vec![item.into()], scope).await
    })
    .await
}

// R-X1, R-W6, R-Q10, R-Q6: a soft-delete cascade in one transaction: item → accounts → everything under them.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_an_item_soft_deletes_everything_under_its_accounts() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    for (item, owner, accounts) in
        [("item-1", home.alice, vec!["acc-1", "acc-2"]), ("item-2", home.alice, vec!["acc-3"]), ("item-9", home.carol, vec!["acc-9"])]
    {
        upsert(&db, "items", data! {"item_id": item, "user_id": owner}, Some(ITEM_COLUMNS), None).await?;
        for account in accounts {
            insert(&db, "accounts", data! {"user_id": owner, "account_id": account, "item_id": item, "institution_id": "ins-1"}, &[], false).await?;
            txn(&db, owner, &format!("t-{account}"), account, 10, "2026-03-05").await?;
            insert(
                &db,
                "investment_transactions",
                data! {"user_id": owner, "investment_transaction_id": format!("i-{account}"), "account_id": account, "date": "2026-03-05"},
                &[],
                false,
            )
            .await?;
            insert(
                &db,
                "split_transactions",
                data! {"user_id": owner, "transaction_id": format!("t-{account}"), "account_id": account, "date": "2026-03-05"},
                &[],
                false,
            )
            .await?;
            upsert(
                &db,
                "snapshots",
                data! {
                    "snapshot_id": format!("{account}-20260305"), "user_id": owner, "snapshot_date": "2026-03-05T00:00:00Z",
                    "snapshot_type": "account_balance", "account_id": account,
                },
                None,
                None,
            )
            .await?;
            upsert(
                &db,
                "snapshots",
                data! {
                    "snapshot_id": format!("holding-{account}-sec-1-20260305"), "user_id": owner, "snapshot_date": "2026-03-05T00:00:00Z",
                    "snapshot_type": "holding", "holding_account_id": account, "holding_security_id": "sec-1",
                },
                None,
                None,
            )
            .await?;
            upsert(
                &db,
                "holdings",
                data! {"user_id": owner, "holding_id": format!("{account}-sec-1"), "account_id": account, "security_id": "sec-1"},
                None,
                None,
            )
            .await?;
        }
    }
    txn(&db, home.alice, "t-acc-3-other", "acc-3", 10, "2026-03-05").await?;
    for (n, a, b) in [(1, "t-acc-1", "t-acc-3"), (2, "t-acc-3", "t-acc-2"), (3, "t-acc-3", "t-acc-3-other"), (4, "t-acc-9", "t-acc-3")] {
        let owner = if n == 4 { home.carol } else { home.alice };
        db.insert(
            Insert::into("transaction_pairs")
                .set("pair_id", uuid(n))
                .set("user_id", owner)
                .set("transaction_id_a", a)
                .set("transaction_id_b", b)
                .set("status", "confirmed"),
        )
        .await?;
    }
    // The live rows of each table, by the account they hang off.
    let live_under = |account: &'static str| {
        let db = &db;
        async move {
            let mut live_rows = Vec::new();
            for (table, column) in [
                ("accounts", "account_id"),
                ("transactions", "account_id"),
                ("investment_transactions", "account_id"),
                ("split_transactions", "account_id"),
                ("snapshots", "account_id"),
                ("snapshots", "holding_account_id"),
                ("holdings", "account_id"),
            ] {
                live_rows.push(db.query(find(Query::table(table), &Filters::new(Scope::Everyone).eq(column, account))).await?.len());
            }
            Ok::<_, DbError>(live_rows)
        }
    };
    let live_pairs =
        || async { Ok::<_, DbError>(column::<Uuid>(&db.query(find(Query::table("transaction_pairs"), &Filters::new(Scope::Everyone))).await?, "pair_id")) };
    assert_eq!(live_under("acc-1").await?, [1, 1, 1, 1, 1, 1, 1]);

    // Someone outside the group deletes nothing at all.
    assert_eq!(delete_item(&db, home.carol, "item-1").await?, 0);
    assert_eq!((live_under("acc-1").await?, live_under("acc-2").await?), (vec![1; 7], vec![1; 7]));
    assert_eq!(live_pairs().await?.len(), 4);

    // Bob, of the group, deletes Alice's item.
    assert_eq!(delete_item(&db, home.bob, "item-1").await?, 1);
    assert_eq!((live_under("acc-1").await?, live_under("acc-2").await?), (vec![0; 7], vec![0; 7]));
    assert!(find_one(&db, Query::table("items").key(["item-1"]), &Filters::new(Scope::Everyone)).await?.is_none());
    // The other item of hers, and Carol's, are as they were.
    assert_eq!(live_under("acc-3").await?, [1, 2, 1, 1, 1, 1, 1]);
    assert_eq!(live_under("acc-9").await?, [1, 1, 1, 1, 1, 1, 1]);
    // The pairs that named a transaction of the item's accounts, on
    // either side, went too; the pair between two transactions of the
    // remaining account, and Carol's own pair, did not.
    assert_eq!(live_pairs().await?, [uuid(3), uuid(4)]);
    // Everything the cascade touched carries one timestamp.
    let mut stamps = BTreeSet::new();
    for (table, id) in [
        ("items", "item-1"),
        ("accounts", "acc-1"),
        ("accounts", "acc-2"),
        ("transactions", "t-acc-1"),
        ("holdings", "acc-2-sec-1"),
        ("snapshots", "holding-acc-1-sec-1-20260305"),
    ] {
        stamps.insert(stored(&db, table, id).await?.get::<Timestamp>("updated")?);
    }
    stamps.insert(stored(&db, "transaction_pairs", uuid(1)).await?.get::<Timestamp>("updated")?);
    assert_eq!(stamps.len(), 1);
    // Deleting it again finds no live account, and flags the item again.
    assert_eq!(delete_item(&db, home.alice, "item-1").await?, 1);
    assert_eq!(live_under("acc-3").await?, [1, 2, 1, 1, 1, 1, 1]);
    Ok(())
}

/// A per-row write helper that swallows a failure and reports it as that
/// row's status, so one bad row does not stop a sync.
async fn upsert_status(db: &impl Db, table: &str, row: Data) -> u16 {
    match upsert(db, table, row, None, None).await {
        Ok(rows) => status(&rows),
        Err(_) => 500,
    }
}

// R-X1, R-W11, R-X4: a failed statement poisons the transaction: when every later error is swallowed too, nothing
// is stored, and the commit says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_swallowed_statement_error_aborts_the_transaction_and_nothing_is_stored() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    upsert(&db, "items", data! {"item_id": "item-1", "user_id": ann, "cursor": "window-1"}, None, None).await?;
    let synced = |id: &'static str, amount: f64| data! {"user_id": ann, "transaction_id": id, "account_id": "acc-1", "amount": amount, "date": "2026-03-05"};
    let batch = || [synced("s-1", 10.0), synced("s-2", 1e15), synced("s-3", 30.0), synced("s-4", 40.0)];

    // On the pool every row is its own statement: the bad row is that
    // row's 500, and the others are stored.
    let mut statuses = Vec::new();
    for row in batch() {
        statuses.push(upsert_status(&db, "transactions", row).await);
    }
    assert_eq!(statuses, [200, 500, 200, 200]);
    assert_eq!(matching(&db, ann, val(true)).await?, ["s-1", "s-3", "s-4"]);
    bulk_hard_delete(&db, "transactions", vec!["s-1".into(), "s-3".into(), "s-4".into()]).await?;

    // The same loop inside a sync's transaction. The second row fails and
    // its error is swallowed as before; but from then on every statement is
    // refused, each refusal swallowed in turn.
    let tx = db.begin().await?;
    let mut statuses = Vec::new();
    for row in batch() {
        statuses.push(upsert_status(&tx, "transactions", row).await);
    }
    assert_eq!(statuses, [200, 500, 500, 500]);
    // So is the write that advances the item's fetch window, and a read.
    assert_eq!(upsert_status(&tx, "items", data! {"item_id": "item-1", "user_id": ann, "cursor": "window-2"}).await, 500);
    assert_eq!(code(tx.query(Query::table("transactions")).await), ErrorCode::TransactionAborted);
    assert_eq!(code(tx.savepoint("too-late").await), ErrorCode::TransactionAborted);
    // Postgres answers the `COMMIT` with a silent rollback. Here the commit
    // says what happened; either way nothing was written, not even the row
    // that had succeeded, and the window did not advance.
    assert_eq!(code(tx.commit().await), ErrorCode::TransactionAborted);
    assert!(matching(&db, ann, val(true)).await?.is_empty());
    assert_eq!(stored(&db, "items", "item-1").await?.get::<String>("cursor")?, "window-1");

    // A statement whose error is not swallowed ends the block at once:
    // `transaction` rolls back and hands the error on.
    let raised = db
        .transaction(|tx| async move {
            let mut statuses = Vec::new();
            for row in batch() {
                statuses.push(upsert_status(&tx, "transactions", row).await);
            }
            upsert(&tx, "items", data! {"item_id": "item-1", "user_id": ann, "cursor": "window-2"}, None, None).await?;
            Ok::<_, DbError>(statuses)
        })
        .await;
    assert_eq!(code(raised), ErrorCode::TransactionAborted);
    assert!(matching(&db, ann, val(true)).await?.is_empty());

    // With good rows only, the same transaction stores them and the
    // window together, visible to every connection once it returns.
    let stored_now = db
        .transaction(|tx| async move {
            let mut statuses = Vec::new();
            for row in [synced("s-1", 10.0), synced("s-3", 30.0)] {
                statuses.push(upsert_status(&tx, "transactions", row).await);
            }
            upsert(&tx, "items", data! {"item_id": "item-1", "user_id": ann, "cursor": "window-2"}, None, None).await?;
            Ok::<_, DbError>(statuses)
        })
        .await?;
    assert_eq!(stored_now, [200, 200]);
    assert_eq!(matching(&db, ann, val(true)).await?, ["s-1", "s-3"]);
    assert_eq!(stored(&db, "items", "item-1").await?.get::<String>("cursor")?, "window-2");
    Ok(())
}

// R-S6, R-W10, R-W11, R-S2: the foreign keys: database cascades exist for an operator deleting rows by hand;
// `RESTRICT` stops the rest; everything else that looks like a reference is unenforced.
#[tokio::test(flavor = "multi_thread")]
async fn hard_deletes_cascade_from_transactions_categories_and_users_and_are_refused_elsewhere() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let home = household(&db).await?;
    let budget: Uuid = insert(&db, "budgets", data! {"user_id": home.alice}, &[], false).await?.one()?.get("budget_id")?;
    let section: Uuid = insert(&db, "sections", data! {"user_id": home.alice, "budget_id": budget}, &[], false).await?.one()?.get("section_id")?;
    let category: Uuid = insert(&db, "categories", data! {"user_id": home.alice, "section_id": section}, &[], false).await?.one()?.get("category_id")?;
    let other: Uuid = insert(&db, "categories", data! {"user_id": home.alice, "section_id": section}, &[], false).await?.one()?.get("category_id")?;
    for id in ["t-1", "t-2", "t-3"] {
        txn(&db, home.alice, id, "acc-nowhere", 10, "2026-03-05").await?;
    }
    for (n, a, b) in [(1, "t-1", "t-2"), (2, "t-2", "t-3"), (3, "t-3", "t-1")] {
        db.insert(
            Insert::into("transaction_pairs")
                .set("pair_id", uuid(n))
                .set("user_id", home.alice)
                .set("transaction_id_a", a)
                .set("transaction_id_b", b)
                .set("status", "suggested"),
        )
        .await?;
    }
    for (transaction, rejected) in [("t-1", category), ("t-1", other), ("t-2", category), ("t-3", other)] {
        db.insert(Insert::into("rejected_categories").set("transaction_id", transaction).set("user_id", home.alice).set("category_id", rejected)).await?;
    }

    // `RESTRICT`: a parent with children cannot be deleted, whichever level
    // it is on. The application never tries; it only ever soft-deletes.
    assert_eq!(code(hard_delete(&db, "budgets", budget).await), ErrorCode::ForeignKeyViolation);
    assert_eq!(code(hard_delete(&db, "sections", section).await), ErrorCode::ForeignKeyViolation);
    assert_eq!(code(hard_delete(&db, "users", home.alice).await), ErrorCode::ForeignKeyViolation);
    assert!(db.get("users", [home.alice]).await?.is_some());

    // `CASCADE` from a transaction: its pairs, on either side, and its
    // rejections go with it. The count is of the transaction alone.
    let deleted = db.delete(Delete::from("transactions").key(["t-1"]).returning(["transaction_id", "amount"])).await?;
    assert_eq!((deleted.affected(), deleted.one()?.get::<Decimal>("amount")?), (1, dec("10")));
    assert_eq!(column::<Uuid>(&db.query(Query::table("transaction_pairs")).await?, "pair_id"), [uuid(2)]);
    assert_eq!(texts_of(&db.query(Query::table("rejected_categories")).await?, "transaction_id"), ["t-2", "t-3"]);
    // `CASCADE` from a category: the rejections that name it.
    assert!(hard_delete(&db, "categories", category).await?);
    assert_eq!(texts_of(&db.query(Query::table("rejected_categories")).await?, "transaction_id"), ["t-3"]);

    // `CASCADE` from a user reaches their keys, membership, invitations
    // and rejections; a user who owns nothing else can be deleted.
    let dave = insert(&db, "users", data! {"username": "dave", "email": "dave@example.com"}, &[], false).await?.one()?.get::<Uuid>("user_id")?;
    insert(&db, "api_keys", data! {"user_id": dave, "name": "key", "key_hash": "d".repeat(64), "key_prefix": "key_d"}, &[], false).await?;
    share(&db, uuid(0x7001), &[dave]).await?;
    invite(&db, dave, home.alice).await?;
    invite(&db, home.bob, dave).await?;
    invite(&db, home.bob, home.carol).await?;
    invite_email(&db, dave, "friend@example.com", ts("2030-01-01T00:00:00Z")).await?;
    invite_email(&db, home.alice, "dave@example.com", ts("2030-01-01T00:00:00Z")).await?;
    db.insert(Insert::into("rejected_categories").set("transaction_id", "t-2").set("user_id", dave).set("category_id", other)).await?;
    assert!(hard_delete(&db, "users", dave).await?);
    assert!(db.query(Query::table("api_keys")).await?.is_empty());
    assert!(db.get("sharing_members", [dave]).await?.is_none());
    assert_eq!(db.query(Query::table("sharing_invitations")).await?.len(), 1);
    // An invitation to an address is not tied to whoever holds it.
    assert_eq!(texts_of(&db.query(Query::table("sharing_email_invitations")).await?, "invitee_email"), ["dave@example.com"]);
    assert_eq!(texts_of(&db.query(Query::table("rejected_categories")).await?, "transaction_id"), ["t-3"]);

    // What only looks like a reference is not one: a transaction on an
    // account that does not exist, a label naming a deleted category, a
    // session of a user who is gone, an account deleted from under its
    // transactions.
    update(&db, "transactions", "t-2", data! {"label_category_id": category, "label_budget_id": uuid(404)}, Scope::Owner(home.alice), vec![]).await?;
    upsert(&db, "sessions", data! {"session_id": "s-1", "user_user_id": dave}, None, None).await?;
    account(&db, home.alice, "acc-1").await?;
    txn(&db, home.alice, "t-4", "acc-1", 1, "2026-03-05").await?;
    assert!(hard_delete(&db, "accounts", "acc-1").await?);
    assert_eq!(stored(&db, "transactions", "t-4").await?.get::<String>("account_id")?, "acc-1");
    Ok(())
}
