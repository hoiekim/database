# `budget` — how the app uses Postgres today

Inventory of every database behaviour the `budget` app depends on, taken from the repository at `the budget repository` (HEAD `24f5cac`). The Rust server (`src/server-rust`) is treated as primary; the Bun/TypeScript server (`src/server`) was used to cross-check. Nothing under `the budget repository` was modified.

This is a reading of the source, the checked-in schema dump and the docs. Neither server nor a Postgres instance was run, so anything about data volume (section 6) is an estimate, and the deployed database's locale and time zone are not known from the repository.

## How to read this document

Path prefixes used in `file:line` references:

| prefix | stands for |
|---|---|
| `pg/` | `src/server-rust/src/server/postgres/` |
| `repo/` | `src/server-rust/src/server/postgres/repositories/` |
| `ct/` | `src/server-rust/src/server/compute_tools/` |
| `srv/` | `src/server-rust/src/server/` |
| `routes/` | `src/server-rust/src/routes/` |
| `ts/` | `src/server/` (the Bun server) |

Abbreviations used inside SQL:

| token | expands to |
|---|---|
| `LIVE` | `(is_deleted IS NULL OR is_deleted = FALSE)` (with a table alias where the statement uses one) |
| `GROUP($n)` | `user_id = ANY(sharing_user_ids($n))` — the owner column matched against the caller and everyone in their sharing group |
| `NOW` | `CURRENT_TIMESTAMP` |
| `[ … ]` | an optional clause, present only when the caller supplies that filter |

Other conventions:

- "Builder" means SQL generated at run time from the model definitions by `pg/database.rs` and `pg/models/base.rs`. "Raw" means a statement written out by hand.
- Each catalogue entry has an id (`Q-…`). Identical shapes are listed once with every call site.
- Every value crosses the driver as JSON. On the way in, numbers are bound typed (`int4`, `int8`, `float8`) and booleans as `bool`; strings, `null`, arrays and objects are bound **untyped**, so the server infers the type from the column or an explicit cast (`pg/client.rs:81`). On the way out `NUMERIC` and `BIGINT` become 64-bit floats, `DATE` becomes `YYYY-MM-DD`, `TIMESTAMPTZ` becomes an ISO string in UTC truncated to milliseconds, `UUID` a lower-case hyphenated string, arrays JSON arrays, `JSON/JSONB` parsed values (`pg/client.rs:250`). The replacement's typed API has to reproduce those conversions, because `conformance/` records every HTTP exchange of both servers and diffs the two transcripts (generated ids, timestamps and tokens masked).

## Rust and TypeScript: do they differ?

At the database level, no. I extracted every SQL string literal from both trees, normalized whitespace and interpolations, and compared them: 93 literals in TypeScript against 92 in Rust, and every difference was one of interpolation or string-splitting style (`${NAME}` against `{NAME}`, a statement built from two concatenated literals), not a different statement. The generic builders have the same methods with the same generated text (`ts/lib/postgres/database.ts`, `ts/lib/postgres/models/base.ts`). The differences that remain are in the driver and pool:

| | Rust | TypeScript |
|---|---|---|
| driver | `tokio-postgres` behind `deadpool-postgres` | `Bun.sql` |
| pool size | 20 (`pg/client.rs:368`) | 20 (`ts/lib/postgres/client.ts:155`) |
| connect timeout | 2 s | 2 s |
| idle timeout | none configured | 30 s |
| pool growth | on demand | Bun opens all 20 on first use |
| statements | prepared per call with declared parameter types (`prepare_typed`), not cached | `prepare: false` (unnamed statements) |
| waiting for a free connection | no wait timeout configured | Bun's own queueing |
| TS-only unused helpers | — | `prepareQuery`, `buildSelect` exist in `database.ts` and have no caller outside tests |

Functions defined in both servers with no live caller (kept in the catalogue, marked *unused*): `get_investment_transactions` (the only `LIMIT`/`OFFSET` user), `delete_user`, `get_user_by_id`, `search_transactions_by_id`, `get_rejected_categories_for_transactions`, `Table::bulk_hard_delete`.

---

# 1. Schema

Source of truth: the `Table` statics in `pg/models/*.rs` (identical to `ts/lib/postgres/models/*.ts`), cross-checked against `conformance/fixtures/schema.json`, which is a catalog dump (`information_schema.columns`, `pg_indexes`, `pg_constraint`, `pg_proc`) of what a fresh start creates on PostgreSQL 16. The two agree exactly: 23 tables, 253 columns, 78 indexes, 54 constraints, 1 function. The tables below are generated from that dump, so types, nullability and defaults are the server's own wording.

## 1.0 Database-wide objects

| kind | what exists |
|---|---|
| Extensions | `pg_trgm` (`CREATE EXTENSION IF NOT EXISTS pg_trgm`, `pg/initialize.rs:38`). Used only for `similarity(text, text)`. No trigram index is created. |
| SQL functions | one: `sharing_user_ids(viewer UUID) RETURNS UUID[]`, `LANGUAGE sql STABLE PARALLEL SAFE`, re-created with `CREATE OR REPLACE` at every start (`pg/models/sharing.rs:104`, `pg/initialize.rs:45`). Body: `SELECT array_append(ARRAY(SELECT peer.user_id FROM sharing_members peer WHERE peer.user_id <> viewer AND peer.group_id = (SELECT own.group_id FROM sharing_members own WHERE own.user_id = viewer)), viewer)`. It answers the viewer plus every other member of the viewer's group; for a user in no group, just the viewer. |
| Sequences | none. No `SERIAL`, `BIGSERIAL` or identity column anywhere. |
| Triggers | none. |
| Enum / domain / composite types | none. Status-like columns are plain `VARCHAR` with no `CHECK`. |
| CHECK constraints | none. |
| Views, materialized views, generated columns, partitions | none. |
| Index types | btree only. One expression index, which is also the only partial index: `idx_users_lower_email`. No GIN/GiST, no `INCLUDE`, no descending keys. |
| Schemas | everything in `public`. |
| Built-in functions the app calls | `gen_random_uuid()` (column defaults), `CURRENT_TIMESTAMP`, `NOW()`, `lower`, `similarity` (pg_trgm), `hashtext`, `pg_advisory_lock`, `pg_advisory_unlock`, `pg_advisory_xact_lock`, `pg_advisory_xact_lock_shared`, `unnest` (one-array and multi-array table form), `array_append`, `ARRAY(subquery)`, `COALESCE`, `MAX`, `MIN`, `SUM`, `COUNT`, `ABS`, `SIGN`, `EXTRACT(DAY FROM …)`, `to_char`, `AT TIME ZONE`, `SUBSTRING(text FROM regex)`, `CAST`, regex match `~`, `LIKE`. |

Server-generated values, in one place:

| value | how | where relied on |
|---|---|---|
| UUID primary keys | `DEFAULT gen_random_uuid()` | `users.user_id`, `split_transactions.split_transaction_id`, `budgets.budget_id`, `sections.section_id`, `categories.category_id`, `charts.chart_id`, `api_keys.key_id`, `sharing_invitations.invitation_id`, `sharing_email_invitations.invitation_id`. The app never supplies these on create and reads them from `RETURNING`. |
| `updated` | every builder `INSERT`/`UPDATE`/upsert/soft delete writes `updated = CURRENT_TIMESTAMP` explicitly; hand-written `INSERT`s into `transaction_pairs`, `recurrings`, `split_transactions` rely on the column default instead | all 22 tables that have the column (all except `rejected_categories`) |
| other timestamps | column default `CURRENT_TIMESTAMP`, never set by the app | `sessions.created_at`, `transaction_pairs.created_at`, `recurrings.created_at`, `api_keys.created_at`, `rejected_categories.rejected_at` (also set to `CURRENT_TIMESTAMP` on conflict) |
| soft-delete flag and other defaults | column default | `is_deleted` FALSE on every soft-deletable table; `accounts.hide/archived`, `accounts.graph_options_*`, `transactions.pending`, `transactions.source`, `investment_transactions.source`, `recurrings.transaction_ids` (`'{}'`), `recurrings.splits` (`'[]'`), `api_keys.scopes` (`'{}'`), `split_transactions.amount` (0) and `custom_name` (`''`), `budgets/sections/categories/charts.name` (`'Unnamed'`), `budgets.iso_currency_code` (`'USD'`), `roll_over` FALSE |

App-generated keys (the database only stores them): `sessions.session_id`, `transaction_pairs.pair_id`, `recurrings.recurring_id`, `sharing_members.group_id` (UUID v4 made in the app); every `VARCHAR` primary key (provider ids, `manual-<uuid>`, and deterministic composites such as `<account>-<YYYYMMDD>`).

Foreign-key summary (the complete list; everything else that looks like a reference is unenforced):

| child column | parent | on delete |
|---|---|---|
| `items.user_id`, `accounts.user_id`, `holdings.user_id`, `transactions.user_id`, `transaction_pairs.user_id`, `recurrings.user_id`, `investment_transactions.user_id`, `split_transactions.user_id`, `budgets.user_id`, `sections.user_id`, `categories.user_id`, `charts.user_id` | `users.user_id` | RESTRICT |
| `sections.budget_id` | `budgets.budget_id` | RESTRICT |
| `categories.section_id` | `sections.section_id` | RESTRICT |
| `transaction_pairs.transaction_id_a`, `transaction_pairs.transaction_id_b` | `transactions.transaction_id` | CASCADE |
| `rejected_categories.transaction_id` | `transactions.transaction_id` | CASCADE |
| `rejected_categories.category_id` | `categories.category_id` | CASCADE |
| `rejected_categories.user_id`, `api_keys.user_id`, `sharing_members.user_id`, `sharing_invitations.inviter_user_id`, `sharing_invitations.invitee_user_id`, `sharing_email_invitations.inviter_user_id` | `users.user_id` | CASCADE |

No foreign key declares `ON UPDATE`, so all are the default `NO ACTION`; the app never updates a referenced key. Not enforced at all: `sessions.user_user_id`, `snapshots.user_id`, every `account_id`, `item_id`, `institution_id`, `security_id`, `transaction_id` on child tables other than the two above, and every `label_budget_id` / `label_category_id`.

Soft delete: 14 tables carry `is_deleted` (`users`, `items`, `accounts`, `holdings`, `transactions`, `transaction_pairs`, `recurrings`, `investment_transactions`, `split_transactions`, `budgets`, `sections`, `categories`, `snapshots`, `charts`). The other 9 (`sessions`, `institutions`, `securities`, `api_keys`, `rejected_categories`, `cpi_snapshots`, `sharing_members`, `sharing_invitations`, `sharing_email_invitations`) do not. The flag is nullable and the predicate everywhere is `is_deleted IS NULL OR is_deleted = FALSE`, so NULL means live.

## 1.1 – 1.23 Tables

Creation order (a table comes after the ones it references): `users`, `sessions`, `institutions`, `securities`, `items`, `accounts`, `holdings`, `transactions`, `transaction_pairs`, `recurrings`, `investment_transactions`, `split_transactions`, `budgets`, `sections`, `categories`, `snapshots`, `charts`, `api_keys`, `rejected_categories`, `cpi_snapshots`, `sharing_members`, `sharing_invitations`, `sharing_email_invitations` (`pg/models/mod.rs:38`).

In the tables below "generated" says what the server fills in when the column is left out of an `INSERT`.

### 1.1 `users`

Declared in `pg/models/user.rs:28`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `user_id` | UUID | NOT NULL | `gen_random_uuid()` | server UUID when omitted |
| 2 | `username` | VARCHAR(255) | NOT NULL | — |  |
| 3 | `password` | VARCHAR(255) | nullable | — |  |
| 4 | `email` | VARCHAR(255) | nullable | — |  |
| 5 | `expiry` | TIMESTAMPTZ | nullable | — |  |
| 6 | `token` | VARCHAR(255) | nullable | — |  |
| 7 | `auth_sub` | TEXT | nullable | — |  |
| 8 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 9 | `is_deleted` | BOOLEAN | nullable | `false` |  |

Constraints:
- PK `users_pkey`: `PRIMARY KEY (user_id)`
- UNIQUE `users_auth_sub_key`: `UNIQUE (auth_sub)`
- UNIQUE `users_username_key`: `UNIQUE (username)`

Indexes (all btree):
- `idx_users_lower_email`: UNIQUE btree `(lower((email)::text)) WHERE (email IS NOT NULL)`
- `users_auth_sub_key`: UNIQUE btree `(auth_sub)` (backs the constraint of the same name)
- `users_pkey`: UNIQUE btree `(user_id)` (backs the constraint of the same name)
- `users_username_key`: UNIQUE btree `(username)` (backs the constraint of the same name)

Notes:
- `user_id` is server-generated: neither `write_user` (create path) nor `create_sso_user` supplies it; it comes back through `RETURNING`.
- `username` is unique, case-sensitive. `auth_sub` (identity-provider subject) is unique among non-NULLs. `email` is unique **ignoring case** among non-NULLs through the partial expression index `idx_users_lower_email` — single sign-on account linking and email invitations rely on it.
- Soft-deletable (`is_deleted`), but nothing in either server calls `delete_user`; a soft-deleted row would keep occupying its username / auth_sub / email keys.
- `expiry` and `token` are never read or written by current code (legacy columns the migration keeps). `password` holds a password hash, NULL for SSO-created users.

### 1.2 `sessions`

Declared in `pg/models/session.rs:50`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `session_id` | VARCHAR(255) | NOT NULL | — |  |
| 2 | `user_user_id` | UUID | nullable | — |  |
| 3 | `user_username` | VARCHAR(255) | nullable | — |  |
| 4 | `cookie_original_max_age` | BIGINT | nullable | — |  |
| 5 | `cookie_max_age` | BIGINT | nullable | — |  |
| 6 | `cookie_signed` | BOOLEAN | nullable | — |  |
| 7 | `cookie_expires` | TIMESTAMPTZ | nullable | — |  |
| 8 | `cookie_http_only` | BOOLEAN | nullable | — |  |
| 9 | `cookie_path` | TEXT | nullable | — |  |
| 10 | `cookie_domain` | TEXT | nullable | — |  |
| 11 | `cookie_secure` | BOOLEAN | nullable | — |  |
| 12 | `cookie_same_site` | VARCHAR(50) | nullable | — |  |
| 13 | `created_at` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 14 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 15 | `auth_sid` | TEXT | nullable | — |  |
| 16 | `auth_refresh_token` | TEXT | nullable | — |  |
| 17 | `auth_recheck_at` | TIMESTAMPTZ | nullable | — |  |

Constraints:
- PK `sessions_pkey`: `PRIMARY KEY (session_id)`

Indexes (all btree):
- `idx_sessions_auth_sid`: btree `(auth_sid)`
- `idx_sessions_user_user_id`: btree `(user_user_id)`
- `sessions_pkey`: UNIQUE btree `(session_id)` (backs the constraint of the same name)

Notes:
- `session_id` is an app-generated UUID v4 stored as text. No FK to `users` (`user_user_id` is a bare UUID). No soft delete: rows are hard-deleted.
- `cookie_original_max_age` / `cookie_max_age` are written as decimal *text* into BIGINT and read back as floats. `created_at` is only ever set by its default.
- `auth_*` columns are only present on single-sign-on sessions; a write that omits them must leave the stored values untouched (partial-column upsert).

### 1.3 `institutions`

Declared in `pg/models/institution.rs:10`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `institution_id` | VARCHAR(255) | NOT NULL | — |  |
| 2 | `name` | VARCHAR(255) | nullable | — |  |
| 3 | `raw` | JSONB | nullable | — |  |
| 4 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |

Constraints:
- PK `institutions_pkey`: `PRIMARY KEY (institution_id)`

Indexes (all btree):
- `institutions_pkey`: UNIQUE btree `(institution_id)` (backs the constraint of the same name)

Notes:
- Global table (no owner). Id is the provider's institution id. `raw` keeps the whole provider payload. No soft delete.

### 1.4 `securities`

Declared in `pg/models/security.rs:8`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `security_id` | VARCHAR(255) | NOT NULL | — |  |
| 2 | `name` | VARCHAR(255) | nullable | — |  |
| 3 | `ticker_symbol` | VARCHAR(50) | nullable | — |  |
| 4 | `type` | VARCHAR(50) | nullable | — |  |
| 5 | `close_price` | NUMERIC(15,6) | nullable | — |  |
| 6 | `close_price_as_of` | DATE | nullable | — |  |
| 7 | `iso_currency_code` | VARCHAR(10) | nullable | — |  |
| 8 | `isin` | VARCHAR(50) | nullable | — |  |
| 9 | `cusip` | VARCHAR(50) | nullable | — |  |
| 10 | `raw` | JSONB | nullable | — |  |
| 11 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |

Constraints:
- PK `securities_pkey`: `PRIMARY KEY (security_id)`

Indexes (all btree):
- `securities_pkey`: UNIQUE btree `(security_id)` (backs the constraint of the same name)

Notes:
- Global table shared by every user (no owner column). Id is the provider's or an app-generated random id. `raw` JSONB is read back field by field in the app (`is_cash_equivalent`, `sector`, …), so it must round-trip faithfully.
- `close_price` is NUMERIC(15,6); `close_price_as_of` is a DATE.

### 1.5 `items`

Declared in `pg/models/item.rs:8`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `item_id` | VARCHAR(255) | NOT NULL | — |  |
| 2 | `user_id` | UUID | NOT NULL | — |  |
| 3 | `access_token` | VARCHAR(255) | nullable | — |  |
| 4 | `institution_id` | VARCHAR(255) | nullable | — |  |
| 5 | `available_products` | TEXT[] | nullable | — |  |
| 6 | `cursor` | TEXT | nullable | — |  |
| 7 | `status` | VARCHAR(50) | nullable | — |  |
| 8 | `status_reason` | VARCHAR(50) | nullable | — |  |
| 9 | `provider` | VARCHAR(50) | nullable | — |  |
| 10 | `last_sync_status` | VARCHAR(20) | nullable | — |  |
| 11 | `last_sync_at` | TIMESTAMPTZ | nullable | — |  |
| 12 | `last_sync_error` | TEXT | nullable | — |  |
| 13 | `raw` | JSONB | nullable | — |  |
| 14 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 15 | `is_deleted` | BOOLEAN | nullable | `false` |  |

Constraints:
- PK `items_pkey`: `PRIMARY KEY (item_id)`
- FK `items_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE RESTRICT`

Indexes (all btree):
- `idx_items_institution_id`: btree `(institution_id)`
- `idx_items_user_id`: btree `(user_id)`
- `items_pkey`: UNIQUE btree `(item_id)` (backs the constraint of the same name)

Notes:
- Id is the Plaid item id, or an app-generated id for SimpleFin/manual items. `access_token` is stored in clear text. `available_products` is a TEXT[]; `cursor` is Plaid's opaque sync cursor.
- `updated` is load-bearing beyond delta sync: the SimpleFin sync derives its fetch window from the item's `updated` (`ct/sync_simple_fin.rs:176`), and every builder write to the row re-stamps it.

### 1.6 `accounts`

Declared in `pg/models/account.rs:8`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `account_id` | VARCHAR(255) | NOT NULL | — |  |
| 2 | `user_id` | UUID | NOT NULL | — |  |
| 3 | `item_id` | VARCHAR(255) | NOT NULL | — |  |
| 4 | `institution_id` | VARCHAR(255) | NOT NULL | — |  |
| 5 | `name` | VARCHAR(255) | nullable | — |  |
| 6 | `type` | VARCHAR(50) | nullable | — |  |
| 7 | `subtype` | VARCHAR(100) | nullable | — |  |
| 8 | `balances_available` | NUMERIC(15,2) | nullable | — |  |
| 9 | `balances_current` | NUMERIC(15,2) | nullable | — |  |
| 10 | `balances_limit` | NUMERIC(15,2) | nullable | — |  |
| 11 | `balances_iso_currency_code` | VARCHAR(10) | nullable | — |  |
| 12 | `custom_name` | TEXT | nullable | — |  |
| 13 | `hide` | BOOLEAN | nullable | `false` |  |
| 14 | `archived` | BOOLEAN | nullable | `false` |  |
| 15 | `label_budget_id` | UUID | nullable | — |  |
| 16 | `graph_options_use_snapshots` | BOOLEAN | nullable | `true` |  |
| 17 | `graph_options_use_holding_snapshots` | BOOLEAN | nullable | `true` |  |
| 18 | `graph_options_use_transactions` | BOOLEAN | nullable | `true` |  |
| 19 | `plan_fund_mappings` | JSONB | nullable | — |  |
| 20 | `raw` | JSONB | nullable | — |  |
| 21 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 22 | `is_deleted` | BOOLEAN | nullable | `false` |  |

Constraints:
- PK `accounts_pkey`: `PRIMARY KEY (account_id)`
- FK `accounts_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE RESTRICT`

Indexes (all btree):
- `accounts_pkey`: UNIQUE btree `(account_id)` (backs the constraint of the same name)
- `idx_accounts_institution_id`: btree `(institution_id)`
- `idx_accounts_item_id`: btree `(item_id)`
- `idx_accounts_user_id`: btree `(user_id)`

Notes:
- Id is the provider's account id or `manual-<uuid v4>` minted by the app. `item_id`, `institution_id` and `label_budget_id` have **no** foreign key.
- `plan_fund_mappings` is a JSONB *object* keyed by fund name (`{name: {security_id, restated}}`); the API returns its entries in the order the database returns the keys (Postgres jsonb key order: shorter keys first, then bytewise).
- Boolean defaults (`hide`/`archived` FALSE, `graph_options_*` TRUE) are relied on by inserts that omit them.

### 1.7 `holdings`

Declared in `pg/models/holding.rs:8`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `holding_id` | VARCHAR(255) | NOT NULL | — |  |
| 2 | `user_id` | UUID | NOT NULL | — |  |
| 3 | `account_id` | VARCHAR(255) | NOT NULL | — |  |
| 4 | `security_id` | VARCHAR(255) | NOT NULL | — |  |
| 5 | `institution_price` | NUMERIC(15,6) | nullable | — |  |
| 6 | `institution_price_as_of` | DATE | nullable | — |  |
| 7 | `institution_value` | NUMERIC(15,2) | nullable | — |  |
| 8 | `cost_basis` | NUMERIC(15,2) | nullable | — |  |
| 9 | `quantity` | NUMERIC(15,6) | nullable | — |  |
| 10 | `iso_currency_code` | VARCHAR(10) | nullable | — |  |
| 11 | `raw` | JSONB | nullable | — |  |
| 12 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 13 | `is_deleted` | BOOLEAN | nullable | `false` |  |

Constraints:
- PK `holdings_pkey`: `PRIMARY KEY (holding_id)`
- FK `holdings_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE RESTRICT`

Indexes (all btree):
- `holdings_pkey`: UNIQUE btree `(holding_id)` (backs the constraint of the same name)
- `idx_holdings_account_id`: btree `(account_id)`
- `idx_holdings_security_id`: btree `(security_id)`
- `idx_holdings_user_id`: btree `(user_id)`

Notes:
- Id is the provider's, or `<account_id>-<security_id>` derived by the app. `account_id` / `security_id` have no FK.

### 1.8 `transactions`

Declared in `pg/models/transaction.rs:8`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `transaction_id` | VARCHAR(255) | NOT NULL | — |  |
| 2 | `user_id` | UUID | NOT NULL | — |  |
| 3 | `account_id` | VARCHAR(255) | NOT NULL | — |  |
| 4 | `name` | TEXT | nullable | — |  |
| 5 | `merchant_name` | TEXT | nullable | — |  |
| 6 | `amount` | NUMERIC(15,2) | nullable | — |  |
| 7 | `iso_currency_code` | VARCHAR(10) | nullable | — |  |
| 8 | `date` | DATE | NOT NULL | — |  |
| 9 | `pending` | BOOLEAN | nullable | `false` |  |
| 10 | `pending_transaction_id` | VARCHAR(255) | nullable | — |  |
| 11 | `payment_channel` | TEXT | nullable | — |  |
| 12 | `location_country` | TEXT | nullable | — |  |
| 13 | `location_region` | TEXT | nullable | — |  |
| 14 | `location_city` | TEXT | nullable | — |  |
| 15 | `label_budget_id` | UUID | nullable | — |  |
| 16 | `label_category_id` | UUID | nullable | — |  |
| 17 | `label_memo` | TEXT | nullable | — |  |
| 18 | `label_category_confidence` | DOUBLE PRECISION (declared FLOAT) | nullable | — |  |
| 19 | `raw` | JSONB | nullable | — |  |
| 20 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 21 | `is_deleted` | BOOLEAN | nullable | `false` |  |
| 22 | `source` | VARCHAR(20) | NOT NULL | `'plaid'::character varying` |  |

Constraints:
- PK `transactions_pkey`: `PRIMARY KEY (transaction_id)`
- FK `transactions_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE RESTRICT`

Indexes (all btree):
- `idx_transactions_account_id`: btree `(account_id)`
- `idx_transactions_date`: btree `(date)`
- `idx_transactions_pending`: btree `(pending)`
- `idx_transactions_user_id_transaction_id`: btree `(user_id, transaction_id)`
- `idx_transactions_user_id_updated`: btree `(user_id, updated)`
- `transactions_pkey`: UNIQUE btree `(transaction_id)` (backs the constraint of the same name)

Notes:
- Id is the provider's transaction id or `manual-<uuid v4>`. `account_id`, `label_budget_id`, `label_category_id`, `pending_transaction_id` have no FK.
- `amount` NUMERIC(15,2): positive = money out, negative = money in. `date` is a calendar DATE (no zone).
- `label_category_confidence` (float8) is a 4-state marker: NULL unlabeled, 0 rejected/cleared, 0<c<1 engine suggestion, 1 user-confirmed. Several statements are compare-and-swap on it.
- `raw` JSONB holds the whole provider transaction (1-3 KB typical for Plaid) and is read inside SQL: `raw->'personal_finance_category'->>'primary'`.
- `source` defaults to `'plaid'`; manual rows write `'manual'`.

### 1.9 `transaction_pairs`

Declared in `pg/models/transaction_pair.rs:7`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `pair_id` | UUID | NOT NULL | — |  |
| 2 | `user_id` | UUID | NOT NULL | — |  |
| 3 | `transaction_id_a` | VARCHAR(255) | NOT NULL | — |  |
| 4 | `transaction_id_b` | VARCHAR(255) | NOT NULL | — |  |
| 5 | `status` | VARCHAR(20) | NOT NULL | — |  |
| 6 | `created_at` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 7 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 8 | `is_deleted` | BOOLEAN | nullable | `false` |  |

Constraints:
- PK `transaction_pairs_pkey`: `PRIMARY KEY (pair_id)`
- UNIQUE `transaction_pairs_pair_unique`: `UNIQUE (transaction_id_a, transaction_id_b)`
- FK `transaction_pairs_transaction_id_a_fkey`: `FOREIGN KEY (transaction_id_a) REFERENCES transactions(transaction_id) ON DELETE CASCADE`
- FK `transaction_pairs_transaction_id_b_fkey`: `FOREIGN KEY (transaction_id_b) REFERENCES transactions(transaction_id) ON DELETE CASCADE`
- FK `transaction_pairs_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE RESTRICT`

Indexes (all btree):
- `idx_transaction_pairs_transaction_id_a`: btree `(transaction_id_a)`
- `idx_transaction_pairs_transaction_id_b`: btree `(transaction_id_b)`
- `idx_transaction_pairs_user_id_updated`: btree `(user_id, updated)`
- `transaction_pairs_pair_unique`: UNIQUE btree `(transaction_id_a, transaction_id_b)` (backs the constraint of the same name)
- `transaction_pairs_pkey`: UNIQUE btree `(pair_id)` (backs the constraint of the same name)

Notes:
- `pair_id` is an app-generated UUID v4 (no default). `(transaction_id_a, transaction_id_b)` is stored in a canonical order computed **by the app** (JavaScript string `<`, i.e. UTF-16 code-unit order), and is unique including soft-deleted and rejected rows — those rows are tombstones the upsert revives.
- `status` is free text: `suggested` / `confirmed` / `rejected` (no CHECK). `created_at`, `updated`, `is_deleted` defaults are relied on by the hand-written INSERTs.
- Both transaction FKs are `ON DELETE CASCADE` and are *enforced on insert* (a pair cannot name a missing transaction).

### 1.10 `recurrings`

Declared in `pg/models/recurring.rs:12`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `recurring_id` | UUID | NOT NULL | — |  |
| 2 | `user_id` | UUID | NOT NULL | — |  |
| 3 | `merchant_key` | TEXT | NOT NULL | — |  |
| 4 | `merchant_name` | TEXT | NOT NULL | — |  |
| 5 | `cadence` | VARCHAR(20) | NOT NULL | — |  |
| 6 | `is_income` | BOOLEAN | NOT NULL | — |  |
| 7 | `status` | VARCHAR(20) | NOT NULL | — |  |
| 8 | `transaction_ids` | TEXT[] | NOT NULL | `'{}'::text[]` |  |
| 9 | `label_budget_id` | UUID | nullable | — |  |
| 10 | `label_category_id` | UUID | nullable | — |  |
| 11 | `splits` | JSONB | NOT NULL | `'[]'::jsonb` |  |
| 12 | `splits_since` | DATE | nullable | — |  |
| 13 | `created_at` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 14 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 15 | `is_deleted` | BOOLEAN | nullable | `false` |  |

Constraints:
- PK `recurrings_pkey`: `PRIMARY KEY (recurring_id)`
- UNIQUE `recurrings_user_merchant_direction_unique`: `UNIQUE (user_id, merchant_key, is_income)`
- FK `recurrings_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE RESTRICT`

Indexes (all btree):
- `idx_recurrings_user_id_merchant_key`: btree `(user_id, merchant_key)`
- `idx_recurrings_user_id_updated`: btree `(user_id, updated)`
- `recurrings_pkey`: UNIQUE btree `(recurring_id)` (backs the constraint of the same name)
- `recurrings_user_merchant_direction_unique`: UNIQUE btree `(user_id, merchant_key, is_income)` (backs the constraint of the same name)

Notes:
- `recurring_id` is an app-generated UUID v4. UNIQUE `(user_id, merchant_key, is_income)` also covers dismissed and soft-deleted rows: they are the per-merchant denylist / tombstone that `ON CONFLICT` revives.
- `transaction_ids` TEXT[] is the member list (no FK; array overlap, concatenation and `= ANY(column)` are all used on it). `splits` is a JSONB array of `{amount, label:{budget_id, category_id}}` read back raw. `status`: `suggested` / `confirmed` / `dismissed`.

### 1.11 `investment_transactions`

Declared in `pg/models/investment_transaction.rs:8`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `investment_transaction_id` | VARCHAR(255) | NOT NULL | — |  |
| 2 | `user_id` | UUID | NOT NULL | — |  |
| 3 | `account_id` | VARCHAR(255) | NOT NULL | — |  |
| 4 | `security_id` | VARCHAR(255) | nullable | — |  |
| 5 | `date` | DATE | NOT NULL | — |  |
| 6 | `name` | TEXT | nullable | — |  |
| 7 | `amount` | NUMERIC(15,2) | nullable | — |  |
| 8 | `quantity` | NUMERIC(15,6) | nullable | — |  |
| 9 | `price` | NUMERIC(15,6) | nullable | — |  |
| 10 | `iso_currency_code` | VARCHAR(10) | nullable | — |  |
| 11 | `type` | TEXT | nullable | — |  |
| 12 | `subtype` | TEXT | nullable | — |  |
| 13 | `label_budget_id` | UUID | nullable | — |  |
| 14 | `label_category_id` | UUID | nullable | — |  |
| 15 | `label_memo` | TEXT | nullable | — |  |
| 16 | `raw` | JSONB | nullable | — |  |
| 17 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 18 | `is_deleted` | BOOLEAN | nullable | `false` |  |
| 19 | `source` | VARCHAR(20) | NOT NULL | `'plaid'::character varying` |  |

Constraints:
- PK `investment_transactions_pkey`: `PRIMARY KEY (investment_transaction_id)`
- FK `investment_transactions_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE RESTRICT`

Indexes (all btree):
- `idx_investment_transactions_account_id`: btree `(account_id)`
- `idx_investment_transactions_date`: btree `(date)`
- `idx_investment_transactions_user_id_investment_transaction_id`: btree `(user_id, investment_transaction_id)`
- `idx_investment_transactions_user_id_updated`: btree `(user_id, updated)`
- `investment_transactions_pkey`: UNIQUE btree `(investment_transaction_id)` (backs the constraint of the same name)

Notes:
- Id is the provider's, `manual-<uuid v4>`, or `plan-[dividend-]<32 hex>[-cash]` (a deterministic id the plan-statement import derives, so re-importing is idempotent through `ON CONFLICT DO NOTHING`).
- `amount` NUMERIC(15,2); `quantity`, `price` NUMERIC(15,6).

### 1.12 `split_transactions`

Declared in `pg/models/split_transaction.rs:8`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `split_transaction_id` | UUID | NOT NULL | `gen_random_uuid()` | server UUID when omitted |
| 2 | `user_id` | UUID | NOT NULL | — |  |
| 3 | `transaction_id` | VARCHAR(255) | NOT NULL | — |  |
| 4 | `account_id` | VARCHAR(255) | NOT NULL | — |  |
| 5 | `amount` | NUMERIC(15,2) | nullable | `0` |  |
| 6 | `date` | DATE | NOT NULL | — |  |
| 7 | `custom_name` | TEXT | nullable | `''::text` |  |
| 8 | `label_budget_id` | UUID | nullable | — |  |
| 9 | `label_category_id` | UUID | nullable | — |  |
| 10 | `label_memo` | TEXT | nullable | — |  |
| 11 | `label_category_confidence` | DOUBLE PRECISION (declared FLOAT) | nullable | — |  |
| 12 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 13 | `is_deleted` | BOOLEAN | nullable | `false` |  |

Constraints:
- PK `split_transactions_pkey`: `PRIMARY KEY (split_transaction_id)`
- FK `split_transactions_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE RESTRICT`

Indexes (all btree):
- `idx_split_transactions_account_id`: btree `(account_id)`
- `idx_split_transactions_transaction_id`: btree `(transaction_id)`
- `idx_split_transactions_user_id_updated`: btree `(user_id, updated)`
- `split_transactions_pkey`: UNIQUE btree `(split_transaction_id)` (backs the constraint of the same name)

Notes:
- `split_transaction_id` is always server-generated (`DEFAULT gen_random_uuid()`), both by the builder insert and by the set-based `INSERT … SELECT` that creates several rows per statement (one fresh UUID per produced row). That statement also relies on the `updated`, `is_deleted` and `custom_name` defaults.
- `transaction_id` / `account_id` have no FK.

### 1.13 `budgets`

Declared in `pg/models/budget.rs:8`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `budget_id` | UUID | NOT NULL | `gen_random_uuid()` | server UUID when omitted |
| 2 | `user_id` | UUID | NOT NULL | — |  |
| 3 | `name` | VARCHAR(255) | nullable | `'Unnamed'::character varying` |  |
| 4 | `iso_currency_code` | VARCHAR(10) | nullable | `'USD'::character varying` |  |
| 5 | `roll_over` | BOOLEAN | nullable | `false` |  |
| 6 | `roll_over_start_date` | DATE | nullable | — |  |
| 7 | `capacities` | JSONB | nullable | — |  |
| 8 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 9 | `is_deleted` | BOOLEAN | nullable | `false` |  |

Constraints:
- PK `budgets_pkey`: `PRIMARY KEY (budget_id)`
- FK `budgets_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE RESTRICT`

Indexes (all btree):
- `budgets_pkey`: UNIQUE btree `(budget_id)` (backs the constraint of the same name)
- `idx_budgets_user_id`: btree `(user_id)`

Notes:
- Id server-generated. `capacities` is a JSONB array written and read whole.

### 1.14 `sections`

Declared in `pg/models/section.rs:8`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `section_id` | UUID | NOT NULL | `gen_random_uuid()` | server UUID when omitted |
| 2 | `user_id` | UUID | NOT NULL | — |  |
| 3 | `budget_id` | UUID | NOT NULL | — |  |
| 4 | `name` | VARCHAR(255) | nullable | `'Unnamed'::character varying` |  |
| 5 | `roll_over` | BOOLEAN | nullable | `false` |  |
| 6 | `roll_over_start_date` | DATE | nullable | — |  |
| 7 | `capacities` | JSONB | nullable | — |  |
| 8 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 9 | `is_deleted` | BOOLEAN | nullable | `false` |  |

Constraints:
- PK `sections_pkey`: `PRIMARY KEY (section_id)`
- FK `sections_budget_id_fkey`: `FOREIGN KEY (budget_id) REFERENCES budgets(budget_id) ON DELETE RESTRICT`
- FK `sections_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE RESTRICT`

Indexes (all btree):
- `idx_sections_budget_id`: btree `(budget_id)`
- `idx_sections_user_id`: btree `(user_id)`
- `sections_pkey`: UNIQUE btree `(section_id)` (backs the constraint of the same name)

Notes:
- Id server-generated. FK to `budgets` is enforced on insert (`RESTRICT`; parents are only ever soft-deleted).

### 1.15 `categories`

Declared in `pg/models/category.rs:8`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `category_id` | UUID | NOT NULL | `gen_random_uuid()` | server UUID when omitted |
| 2 | `user_id` | UUID | NOT NULL | — |  |
| 3 | `section_id` | UUID | NOT NULL | — |  |
| 4 | `name` | VARCHAR(255) | nullable | `'Unnamed'::character varying` |  |
| 5 | `roll_over` | BOOLEAN | nullable | `false` |  |
| 6 | `roll_over_start_date` | DATE | nullable | — |  |
| 7 | `capacities` | JSONB | nullable | — |  |
| 8 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 9 | `is_deleted` | BOOLEAN | nullable | `false` |  |

Constraints:
- PK `categories_pkey`: `PRIMARY KEY (category_id)`
- FK `categories_section_id_fkey`: `FOREIGN KEY (section_id) REFERENCES sections(section_id) ON DELETE RESTRICT`
- FK `categories_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE RESTRICT`

Indexes (all btree):
- `categories_pkey`: UNIQUE btree `(category_id)` (backs the constraint of the same name)
- `idx_categories_section_id`: btree `(section_id)`
- `idx_categories_user_id`: btree `(user_id)`

Notes:
- Id server-generated. FK to `sections` enforced on insert.

### 1.16 `snapshots`

Declared in `pg/models/snapshot.rs:36`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `snapshot_id` | VARCHAR(255) | NOT NULL | — |  |
| 2 | `user_id` | UUID | nullable | — |  |
| 3 | `snapshot_date` | TIMESTAMPTZ | NOT NULL | — |  |
| 4 | `snapshot_type` | VARCHAR(50) | NOT NULL | — |  |
| 5 | `account_id` | VARCHAR(255) | nullable | — |  |
| 6 | `balances_available` | NUMERIC(15,2) | nullable | — |  |
| 7 | `balances_current` | NUMERIC(15,2) | nullable | — |  |
| 8 | `balances_limit` | NUMERIC(15,2) | nullable | — |  |
| 9 | `balances_iso_currency_code` | VARCHAR(10) | nullable | — |  |
| 10 | `security_id` | VARCHAR(255) | nullable | — |  |
| 11 | `close_price` | NUMERIC(15,6) | nullable | — |  |
| 12 | `holding_account_id` | VARCHAR(255) | nullable | — |  |
| 13 | `holding_security_id` | VARCHAR(255) | nullable | — |  |
| 14 | `institution_price` | NUMERIC(15,6) | nullable | — |  |
| 15 | `institution_value` | NUMERIC(15,2) | nullable | — |  |
| 16 | `cost_basis` | NUMERIC(15,2) | nullable | — |  |
| 17 | `quantity` | NUMERIC(15,6) | nullable | — |  |
| 18 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 19 | `is_deleted` | BOOLEAN | nullable | `false` |  |

Constraints:
- PK `snapshots_pkey`: `PRIMARY KEY (snapshot_id)`

Indexes (all btree):
- `idx_snapshots_account_id`: btree `(account_id)`
- `idx_snapshots_holding_account_id`: btree `(holding_account_id)`
- `idx_snapshots_security_id`: btree `(security_id)`
- `idx_snapshots_snapshot_date`: btree `(snapshot_date)`
- `idx_snapshots_snapshot_type`: btree `(snapshot_type)`
- `idx_snapshots_user_id_snapshot_id`: btree `(user_id, snapshot_id)`
- `idx_snapshots_user_id_updated`: btree `(user_id, updated)`
- `snapshots_pkey`: UNIQUE btree `(snapshot_id)` (backs the constraint of the same name)

Notes:
- One table, three kinds by `snapshot_type`: `account_balance` (uses `account_id`, `balances_*`), `security` (uses `security_id`, `close_price`; `user_id` is NULL because prices are global), `holding` (uses `holding_account_id`, `holding_security_id`, `institution_*`, `cost_basis`, `quantity`; its `account_id`/`security_id` stay NULL).
- `user_id` is nullable and has **no** FK. `snapshot_date` is a TIMESTAMPTZ despite its name.
- Ids are deterministic text: `<account_id>-<YYYYMMDD>`, `<security_id>-<YYYYMMDD>`, `<holding_id>-<YYYYMMDD>`, `holding-<account>-<security>-<date>`. Writing the same entity twice in a day is an upsert of one row, and a write onto a tombstoned id revives it (`is_deleted = FALSE` in the upsert).

### 1.17 `charts`

Declared in `pg/models/chart.rs:8`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `chart_id` | UUID | NOT NULL | `gen_random_uuid()` | server UUID when omitted |
| 2 | `user_id` | UUID | nullable | — |  |
| 3 | `name` | VARCHAR(255) | nullable | `'Unnamed'::character varying` |  |
| 4 | `type` | VARCHAR(50) | nullable | — |  |
| 5 | `configuration` | JSONB | nullable | — |  |
| 6 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |
| 7 | `is_deleted` | BOOLEAN | nullable | `false` |  |

Constraints:
- PK `charts_pkey`: `PRIMARY KEY (chart_id)`
- FK `charts_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE RESTRICT`

Indexes (all btree):
- `charts_pkey`: UNIQUE btree `(chart_id)` (backs the constraint of the same name)
- `idx_charts_user_id`: btree `(user_id)`

Notes:
- Id server-generated. `configuration` JSONB: the client sends JSON text, the server parses it before storing and serializes it back to text on read. `user_id` is nullable here (unlike the other owner columns).

### 1.18 `api_keys`

Declared in `pg/models/api_key.rs:8`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `key_id` | UUID | NOT NULL | `gen_random_uuid()` | server UUID when omitted |
| 2 | `user_id` | UUID | NOT NULL | — |  |
| 3 | `name` | VARCHAR(255) | NOT NULL | — |  |
| 4 | `key_hash` | VARCHAR(64) | NOT NULL | — |  |
| 5 | `key_prefix` | VARCHAR(16) | NOT NULL | — |  |
| 6 | `scopes` | TEXT[] | NOT NULL | `'{}'::text[]` |  |
| 7 | `created_at` | TIMESTAMPTZ | NOT NULL | `CURRENT_TIMESTAMP` | server clock when omitted |
| 8 | `last_used_at` | TIMESTAMPTZ | nullable | — |  |
| 9 | `revoked_at` | TIMESTAMPTZ | nullable | — |  |
| 10 | `expires_at` | TIMESTAMPTZ | nullable | — |  |
| 11 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |

Constraints:
- PK `api_keys_pkey`: `PRIMARY KEY (key_id)`
- UNIQUE `api_keys_key_hash_key`: `UNIQUE (key_hash)`
- FK `api_keys_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE CASCADE`

Indexes (all btree):
- `api_keys_key_hash_key`: UNIQUE btree `(key_hash)` (backs the constraint of the same name)
- `api_keys_pkey`: UNIQUE btree `(key_id)` (backs the constraint of the same name)
- `idx_api_keys_key_hash`: btree `(key_hash)`
- `idx_api_keys_user_id`: btree `(user_id)`

Notes:
- Id server-generated. `key_hash` is the SHA-256 of the key as 64 hex characters (text, not bytea), UNIQUE; there is also a redundant plain index on it. `scopes` TEXT[]. Never deleted: revoked by setting `revoked_at`. `created_at` only ever set by its default.

### 1.19 `rejected_categories`

Declared in `pg/models/rejected_category.rs:18`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `transaction_id` | VARCHAR(255) | NOT NULL | — |  |
| 2 | `user_id` | UUID | NOT NULL | — |  |
| 3 | `category_id` | UUID | NOT NULL | — |  |
| 4 | `rejected_at` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |

Constraints:
- PK `rejected_categories_pkey`: `PRIMARY KEY (transaction_id, category_id)`
- FK `rejected_categories_category_id_fkey`: `FOREIGN KEY (category_id) REFERENCES categories(category_id) ON DELETE CASCADE`
- FK `rejected_categories_transaction_id_fkey`: `FOREIGN KEY (transaction_id) REFERENCES transactions(transaction_id) ON DELETE CASCADE`
- FK `rejected_categories_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE CASCADE`

Indexes (all btree):
- `idx_rejected_categories_user_id`: btree `(user_id)`
- `rejected_categories_pkey`: UNIQUE btree `(transaction_id, category_id)` (backs the constraint of the same name)

Notes:
- Composite primary key `(transaction_id, category_id)`. No `updated`, no `is_deleted`; rows are hard-deleted. `rejected_at` is set by its default on insert and by `CURRENT_TIMESTAMP` on conflict.
- All three FKs are `ON DELETE CASCADE` and enforced on insert: rejecting a category id that does not exist fails the statement (the caller swallows the error).
- The generic single-key builder methods refuse this table (`assert_simple_primary_key`), except `query`.

### 1.20 `cpi_snapshots`

Declared in `pg/models/cpi_snapshot.rs:11`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `cpi_snapshot_id` | VARCHAR(255) | NOT NULL | — |  |
| 2 | `series_id` | VARCHAR(50) | NOT NULL | — |  |
| 3 | `snapshot_date` | DATE | NOT NULL | — |  |
| 4 | `index_value` | NUMERIC(15,6) | NOT NULL | — |  |
| 5 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |

Constraints:
- PK `cpi_snapshots_pkey`: `PRIMARY KEY (cpi_snapshot_id)`

Indexes (all btree):
- `cpi_snapshots_pkey`: UNIQUE btree `(cpi_snapshot_id)` (backs the constraint of the same name)
- `idx_cpi_snapshots_series_id`: btree `(series_id)`

Notes:
- Global table. Id is `<series_id>-<YYYY-MM>`. `updated` is re-stamped on every upsert, changed or not, and the refresh job uses it as a 22-hour cadence gate that survives restarts.

### 1.21 `sharing_members`

Declared in `pg/models/sharing.rs:11`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `user_id` | UUID | NOT NULL | — |  |
| 2 | `group_id` | UUID | NOT NULL | — |  |
| 3 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |

Constraints:
- PK `sharing_members_pkey`: `PRIMARY KEY (user_id)`
- FK `sharing_members_user_id_fkey`: `FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE CASCADE`

Indexes (all btree):
- `idx_sharing_members_group_id`: btree `(group_id)`
- `sharing_members_pkey`: UNIQUE btree `(user_id)` (backs the constraint of the same name)

Notes:
- Primary key is `user_id`: a user is in at most one sharing group. `group_id` is an app-generated UUID v4; there is no groups table. Rows are hard-deleted.

### 1.22 `sharing_invitations`

Declared in `pg/models/sharing.rs:32`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `invitation_id` | UUID | NOT NULL | `gen_random_uuid()` | server UUID when omitted |
| 2 | `inviter_user_id` | UUID | NOT NULL | — |  |
| 3 | `invitee_user_id` | UUID | NOT NULL | — |  |
| 4 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |

Constraints:
- PK `sharing_invitations_pkey`: `PRIMARY KEY (invitation_id)`
- UNIQUE `sharing_invitations_pair_unique`: `UNIQUE (inviter_user_id, invitee_user_id)`
- FK `sharing_invitations_invitee_user_id_fkey`: `FOREIGN KEY (invitee_user_id) REFERENCES users(user_id) ON DELETE CASCADE`
- FK `sharing_invitations_inviter_user_id_fkey`: `FOREIGN KEY (inviter_user_id) REFERENCES users(user_id) ON DELETE CASCADE`

Indexes (all btree):
- `idx_sharing_invitations_invitee_user_id`: btree `(invitee_user_id)`
- `sharing_invitations_pair_unique`: UNIQUE btree `(inviter_user_id, invitee_user_id)` (backs the constraint of the same name)
- `sharing_invitations_pkey`: UNIQUE btree `(invitation_id)` (backs the constraint of the same name)

Notes:
- Unanswered invitation to a user; answering deletes the row. UNIQUE `(inviter_user_id, invitee_user_id)` makes a repeated invite a no-op (`ON CONFLICT DO NOTHING`).

### 1.23 `sharing_email_invitations`

Declared in `pg/models/sharing.rs:67`.

| # | column | type | null | default | generated |
|---|---|---|---|---|---|
| 1 | `invitation_id` | UUID | NOT NULL | `gen_random_uuid()` | server UUID when omitted |
| 2 | `inviter_user_id` | UUID | NOT NULL | — |  |
| 3 | `invitee_email` | VARCHAR(255) | NOT NULL | — |  |
| 4 | `expires_at` | TIMESTAMPTZ | NOT NULL | — |  |
| 5 | `updated` | TIMESTAMPTZ | nullable | `CURRENT_TIMESTAMP` | server clock when omitted |

Constraints:
- PK `sharing_email_invitations_pkey`: `PRIMARY KEY (invitation_id)`
- UNIQUE `sharing_email_invitations_pair_unique`: `UNIQUE (inviter_user_id, invitee_email)`
- FK `sharing_email_invitations_inviter_user_id_fkey`: `FOREIGN KEY (inviter_user_id) REFERENCES users(user_id) ON DELETE CASCADE`

Indexes (all btree):
- `idx_sharing_email_invitations_invitee_email`: btree `(invitee_email)`
- `sharing_email_invitations_pair_unique`: UNIQUE btree `(inviter_user_id, invitee_email)` (backs the constraint of the same name)
- `sharing_email_invitations_pkey`: UNIQUE btree `(invitation_id)` (backs the constraint of the same name)

Notes:
- Invitation to an email address, stored lower-cased by the app; matched in SQL against `lower(users.email)`. UNIQUE `(inviter_user_id, invitee_email)` is the upsert's conflict target (renews `expires_at`).

---

# 2. Query catalogue

185 statement shapes are catalogued below: 170 data statements (sections 2.1–2.19), 8 lock and transaction-control statements and 7 DDL/catalog statements (section 2.20). The data statements are instances of 16 generic builder operations (G1–G16, section 2.0) or are written by hand; an entry that lists several variants (the same statement on another column or table) counts once.

## 2.0 The query builder

Most statements are generated. A model is a `Table` static (`pg/models/base.rs:108`): `name`, single-column `primary_key`, `schema` (ordered `(column, definition-text)` pairs that double as the `CREATE TABLE` body), `constraints` (table-level constraint text), `indexes`, per-column runtime type checks, and `supports_soft_delete` (true exactly when the table has `is_deleted`). Rows travel as ordered JSON objects. Two rules run through every generated write:

- **A key absent from the data is left alone; a key present with `null` is written as NULL.** That is how one upsert or update can touch a subset of a row's columns and how inserts fall back to column defaults.
- **`updated` is always stamped by the server** (`CURRENT_TIMESTAMP`), first in the column list of an insert and first in the `SET` list of an update.

Every row read through `query`, `query_one`, `query_by_ids` or `query_in_batches` is validated against the model's column checks and the read fails if a row does not conform (`Table::validate`, `pg/models/base.rs:161`). All reads are `SELECT *`.

### 2.0.1 Generic operations

| id | method (`pg/models/base.rs`) | generated SQL | returns / relied on |
|---|---|---|---|
| **G1** | `query(filters, options, client)` `:216` | `SELECT * FROM t [WHERE c1 AND c2 …] [ORDER BY <text>] [LIMIT $n] [OFFSET $m]` (see 2.0.2) | all rows |
| **G2** | `query_in_batches(filters, options, batch)` `:224` | G1 plus `AND <pk> > $k` (from the second batch on) `ORDER BY <pk> LIMIT $n`. One statement per batch, each on whichever pooled connection is free, batch size 500 (`STREAM_BATCH_SIZE`, `:22`). Stops at the first batch shorter than the limit. | a stream of batches; keyset pagination on the primary key. There is **no** snapshot shared between batches: a row whose key sorts before the cursor and that is inserted mid-stream is missed, one updated mid-stream may be seen in its new state. |
| **G3** | `query_one(filters)` `:278` | G1 with `LIMIT $n` (n = 1) and no `ORDER BY`; always on the pool | first row or none |
| **G4** | `query_by_ids(ids, extra)` `:394` | `SELECT * FROM t WHERE <pk> IN ($1,…,$k) [AND LIVE] [AND col = $… \| AND col = ANY(sharing_user_ids($…))]…` | rows; missing ids are simply absent; empty id list issues no statement |
| **G5** | `insert(data, returning, on_conflict_do_nothing)` `:292` | `INSERT INTO t (updated, c1,…,cn) VALUES (CURRENT_TIMESTAMP, $1,…,$n) [ON CONFLICT DO NOTHING] RETURNING <cols \| pk>` | the returned row, or none when the target-less `ON CONFLICT DO NOTHING` swallowed a violation of **any** unique constraint or unique index |
| **G6** | `update(pk, data, args)` `:305` | `UPDATE t SET updated = CURRENT_TIMESTAMP, c1 = $1,… WHERE <pk> = $k [AND user_id = $… \| AND GROUP($…)] [AND x = $… \| AND x IS NULL \| AND x IS NOT NULL]… [AND LIVE] RETURNING <cols \| pk>` | the returned row or none; "none" is how callers detect not-found / not-owned / deleted / lost a race. A `raw` key in the data is never written by an update. No statement at all when nothing is left to set. `AND LIVE` is on by default for soft-deletable tables. |
| **G7** | `upsert(data, update_columns, client, conflict_columns)` `:329` | `INSERT INTO t (updated, c1,…) VALUES (CURRENT_TIMESTAMP, $1,…) ON CONFLICT (<pk \| conflict columns>) DO UPDATE SET ci = EXCLUDED.ci,…, updated = CURRENT_TIMESTAMP RETURNING *`; with an empty update list: `… ON CONFLICT (<target>) DO NOTHING RETURNING *` | the whole row. `update_columns` defaults to every supplied column except the primary key; an allowlist is intersected with the supplied columns. The conflict target and the primary key are never in the `SET` list. |
| **G8** | `upsert_many(rows, update_columns, client)` `:349` | `INSERT INTO t (updated, c1,…) VALUES (CURRENT_TIMESTAMP, $1,…), (CURRENT_TIMESTAMP, $k,…), … ON CONFLICT (<pk>) DO UPDATE SET … \| DO NOTHING RETURNING *` | returned rows. The app first de-duplicates rows by primary key (last wins) and groups rows by their set of columns, one statement per group. Conflict target is always the primary key. |
| **G9** | `soft_delete(pk, owner)` `:387` | `UPDATE t SET is_deleted = TRUE, updated = CURRENT_TIMESTAMP WHERE <pk> = $1 [AND user_id = $2 \| AND GROUP($2)] RETURNING <pk>` | boolean from the row count. No `LIVE` guard: deleting a deleted row succeeds again and re-stamps `updated`. |
| **G10** | `bulk_soft_delete(ids, extra, client)` `:409` | `UPDATE t SET is_deleted = TRUE, updated = CURRENT_TIMESTAMP WHERE <pk> IN ($1,…,$k) [AND col = $… \| AND GROUP($…)] RETURNING <pk>` | affected-row count |
| **G11** | `bulk_soft_delete_by_column(col, value, owner, client)` `:434` | `UPDATE t SET is_deleted = TRUE, updated = CURRENT_TIMESTAMP WHERE col = $1` — or `col = ANY($1)` when the value is an array (bound as an untyped array literal) — `[AND GROUP($2)] RETURNING <pk>` | affected-row count; an empty array issues no statement |
| **G12** | `bulk_soft_delete_by_related_column(...)` `:461` | `UPDATE t SET is_deleted = TRUE, updated = CURRENT_TIMESTAMP WHERE col IN (SELECT sel FROM src WHERE match = ANY($1)) [AND GROUP($2)] RETURNING <pk>` | affected-row count |
| **G13** | `hard_delete(pk)` `:488` | `DELETE FROM t WHERE <pk> = $1 RETURNING <pk>` | boolean |
| **G14** | `bulk_hard_delete(ids)` `:495` | `DELETE FROM t WHERE <pk> IN ($1,…) RETURNING <pk>` | count — *unused* |
| **G15** | `hard_delete_by_column(col, value)` `:511` | `DELETE FROM t WHERE col = $1 RETURNING <pk>` | count |
| **G16** | `delete_by_condition(col, op, value)` `:519` | `DELETE FROM t WHERE col <op> $1 RETURNING <pk>`, op one of `= <= >= < > !=` | count (only `<=` is used) |

All single-key helpers refuse a table with a composite primary key (`assert_simple_primary_key`, `:191`); only `query` works on `rejected_categories`.

### 2.0.2 What a generated `WHERE` can contain

`build_select_with_filters` (`pg/database.rs:319`) ANDs these, always in this order:

| # | source | condition | notes |
|---|---|---|---|
| 1 | `options.user_id` | `user_id = $n` or `user_id = ANY(sharing_user_ids($n))` | skipped when the id is `""` or null |
| 2 | `options.primary_key` | `<col> = $n` | available, no live caller |
| 3 | each of `filters`, in the order given | `col = $n`; `col IS NULL` when the value is JSON null; `col IS NOT NULL` (`Filter::IsNotNull`, no live caller); `col = ANY(sharing_user_ids($n))` (`Filter::SharedWith`) | equality only |
| 4 | each of `in_filters` | `col IN ($a, $b, …)` — one placeholder per element | an empty list adds nothing (so it does **not** mean "match nothing") |
| 5 | `equals_ignoring_case` | `lower(col) = lower($n)` | only on `users.email` |
| 6 | `date_range` | `col >= $n` and/or `col <= $m` | both bounds inclusive, bound as text; the column is a `DATE` or a `TIMESTAMPTZ` depending on the caller |
| 7 | soft delete | `LIVE` | default on for soft-deletable tables; callers turn it off to receive tombstones |
| 8 | `after` | `col > $n` | keyset cursor, set only by G2 |

Then `ORDER BY <caller text>`, `LIMIT $n`, `OFFSET $m`. The builder cannot express `OR`, `<>`, `LIKE`, joins, aggregates or projections; code that needs those writes SQL by hand (and filters such as "status is not rejected" are applied in the app after a builder read).

`ORDER BY` strings the app actually passes: `<pk>` (keyset), `date DESC`, `snapshot_date`, `snapshot_date ASC`, `snapshot_date, snapshot_id`, `updated DESC`, `pair_id`, `recurring_id`, `transaction_id, rejected_at DESC`.

Extra `WHERE` terms a generated `UPDATE` can carry (`AdditionalWhere`, `pg/database.rs:96`): `col = $n`, `col IS NULL`, `col = ANY(sharing_user_ids($n))`. Used ones: owner scope, `auth_sub IS NULL`, `revoked_at IS NULL`, `label_category_confidence IS NULL`.

### 2.0.3 Which table uses which generic operation

| table | G1 query | G2 batches | G3 one | G4 by ids | G5 insert | G6 update | G7 upsert | G8 upsert many | G9 soft del | G10 bulk soft del | G11 soft del by column | other |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| users | ✓ | | ✓ | | ✓ (do nothing) | ✓ | ✓ | | (unused) | | | joins (raw) |
| sessions | | | ✓ | | | ✓ | ✓ | | | | | G13, G15, G16 |
| institutions | | | | ✓ | | | | ✓ | | | | |
| securities | ✓ | | ✓ | | | | ✓ | | | | | raw join |
| items | ✓ | | ✓ | | | ✓ | ✓ (allowlist) | | | ✓ | | raw join |
| accounts | ✓ | | ✓ | ✓ | ✓ | ✓ | ✓ | | | ✓ | | |
| holdings | ✓ | | | | | | ✓ (allowlist) | | | ✓ | ✓ | raw distinct |
| transactions | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | | | ✓ | ✓ | much raw |
| transaction_pairs | ✓ | | | | | | | | | | ✓ | G12; raw |
| recurrings | ✓ | | | | | ✓ | | | | | | raw |
| investment_transactions | ✓ | ✓ | ✓ | | ✓ | ✓ | ✓ | ✓ (do nothing) | | ✓ | ✓ | raw max |
| split_transactions | ✓ | | | | ✓ | ✓ | | | | ✓ | ✓ | raw |
| budgets | ✓ | | ✓ | ✓ | ✓ | ✓ | | | | ✓ | | |
| sections | ✓ | | ✓ | | ✓ | ✓ | | | | ✓ | | |
| categories | ✓ | | | ✓ | ✓ | ✓ | | | ✓ | | ✓ | |
| snapshots | ✓ | ✓ | ✓ | | | ✓ | ✓ (allowlists) | | ✓ | ✓ | ✓ | raw aggregates |
| charts | ✓ | | | | ✓ | ✓ | | | ✓ | | | |
| api_keys | | | ✓ | | ✓ | ✓ | | | | | | raw list |
| rejected_categories | (unused) | | | | | | | | | | | raw only |
| cpi_snapshots | ✓ | | | | | | | ✓ | | | | |
| sharing_members | ✓ | | | | | | | ✓ (do nothing) | | | | raw |
| sharing_invitations | ✓ | | ✓ | | ✓ (do nothing) | | | | | | | raw |
| sharing_email_invitations | ✓ | | | | | | ✓ (conflict on a unique pair) | | | | | raw |

### 2.0.4 Result codes derived from row counts

Repository writes report an HTTP-like status per row (`pg/database.rs:417`): 200 when the statement returned or affected at least one row, 404 when it affected none, 304 when an update found no live owned row or had nothing to set, 422 when refused before the write, 500 when the statement failed (logged, and the loop carries on with the next row). So `RETURNING` rows and affected-row counts are part of the API contract.

## 2.1 `users`

**Q-USR-1 — lookup by id and/or username (point lookup on PK or unique key).** `repo/users.rs:43` (startup seeding), `:50` (password sign-in), `:87`, `:120` (*unused*), `:124` (bearer auth, `srv/bearer_auth.rs:71`); `repo/sharing.rs:222` (invitee by username).
```sql
SELECT * FROM users WHERE [user_id = $1] [AND username = $2] AND LIVE LIMIT $3
```

**Q-USR-2 — lookup by identity-provider subject.** `repo/users.rs:60` (every SSO sign-in, every introspected bearer key, SSO events).
```sql
SELECT * FROM users WHERE auth_sub = $1 AND LIVE LIMIT $2
```

**Q-USR-3 — case-insensitive email lookup.** `repo/users.rs:92` (link an account by email), `repo/sharing.rs:292` (who holds an invited address). Served by the expression index.
```sql
SELECT * FROM users WHERE lower(email) = lower($1) AND LIVE LIMIT $2
```

**Q-USR-4 — every live user.** `ct/auto_suggest.rs:328`, `ct/detect_recurrings.rs:914`, `ct/detect_transfers.rs:57` (three times an hour).
```sql
SELECT * FROM users WHERE LIVE
```

**Q-USR-5 — create or rewrite a user (G7 on the primary key).** `repo/users.rs:27`, called at every start for `admin` and `demo` (`pg/initialize.rs:89`). Without `user_id` in the data the row is always new and its id comes from the column default; a duplicate `username` then raises a unique violation, which is why the caller looks the user up first.
```sql
INSERT INTO users (updated, username[, password][, user_id]) VALUES (NOW, $1[, $2][, $3])
ON CONFLICT (user_id) DO UPDATE SET username = EXCLUDED.username[, password = EXCLUDED.password], updated = NOW
RETURNING *
```

**Q-USR-6 — link an account to an identity, compare-and-swap.** `repo/users.rs:70`. The row comes back only if the account was still unlinked; a concurrent sign-in that linked it first makes this return nothing.
```sql
UPDATE users SET updated = NOW, auth_sub = $1 WHERE user_id = $2 AND auth_sub IS NULL AND LIVE RETURNING user_id
```

**Q-USR-7 — create an SSO user unless any unique key is taken.** `repo/users.rs:115`, tried with the email and then without (`srv/sso/users.rs:57`). "No row returned" stands for a taken `username`, `auth_sub`, **or** case-insensitive `email` (the partial expression unique index counts as an arbiter of a target-less `ON CONFLICT DO NOTHING`).
```sql
INSERT INTO users (updated, username, auth_sub[, email]) VALUES (NOW, $1, $2[, $3]) ON CONFLICT DO NOTHING RETURNING user_id, username
```

**Q-USR-8 — soft-delete a user (G9).** `repo/users.rs:128`, *unused*.
```sql
UPDATE users SET is_deleted = TRUE, updated = NOW WHERE user_id = $1 RETURNING user_id
```

`users` is also joined or sub-selected by Q-ITM-4, Q-SHR-2, Q-SHR-3, Q-SHR-4, Q-SHR-5, Q-SHR-11, Q-SHR-12 and Q-SHR-16.

## 2.2 `sessions`

**Q-SES-1 — read a session by id.** `repo/session.rs:57`. Runs on **every** request that carries a session cookie (`src/server-rust/src/start.rs:94`) and every 60 s per open event stream when single sign-on is on (`srv/session.rs:190`).
```sql
SELECT * FROM sessions WHERE session_id = $1 LIMIT $2
```

**Q-SES-2 — write a session (G7, every column but the key).** `repo/session.rs:75`. Runs at the end of every signed-in cookie request (`start.rs:120`) unless Q-SES-3 applies. The `auth_*` columns are in the statement only for SSO sessions, so a password session's write leaves them as they are.
```sql
INSERT INTO sessions (updated, session_id, user_user_id, user_username, cookie_original_max_age, cookie_max_age,
  cookie_signed, cookie_expires, cookie_http_only, cookie_path, cookie_domain, cookie_secure, cookie_same_site
  [, auth_sid, auth_refresh_token, auth_recheck_at])
VALUES (NOW, $1, …)
ON CONFLICT (session_id) DO UPDATE SET user_user_id = EXCLUDED.user_user_id, …, updated = NOW
RETURNING *
```

**Q-SES-3 — refresh a session without resurrecting it (G6).** `repo/session.rs:88`. Used instead of Q-SES-2 when single sign-on is on and the row was read on this request, so a session that an identity-provider event deleted mid-request stays deleted.
```sql
UPDATE sessions SET updated = NOW, user_user_id = $1, user_username = $2, cookie_original_max_age = $3, … WHERE session_id = $n RETURNING session_id
```

**Q-SES-4 — store an SSO re-check result (G6).** `repo/session.rs:20`. The caller needs to know whether the row still existed.
```sql
UPDATE sessions SET updated = NOW, auth_sid = $1, auth_refresh_token = $2, auth_recheck_at = $3 WHERE session_id = $4 RETURNING session_id
```

**Q-SES-5 — delete one session (G13).** `repo/session.rs:65` (expired at point of use), `:80` (sign-out, session regeneration).
```sql
DELETE FROM sessions WHERE session_id = $1 RETURNING session_id
```

**Q-SES-6 — delete sessions by a secondary column (G15).** `repo/session.rs:25` (`auth_sid`), `:29` (`user_user_id`); from identity-provider events (`srv/sso/events.rs:30`, `:40`).
```sql
DELETE FROM sessions WHERE auth_sid = $1 RETURNING session_id
DELETE FROM sessions WHERE user_user_id = $1 RETURNING session_id
```

**Q-SES-7 — purge expired sessions (G16, range delete).** `repo/session.rs:15`; a minute after start, then hourly. The bound is the app's clock, not the database's.
```sql
DELETE FROM sessions WHERE cookie_expires <= $1 RETURNING session_id
```

## 2.3 `institutions`

**Q-INS-1 — bulk upsert (G8, multi-row `VALUES`).** `repo/institutions.rs:22`. The returned rows decide each input's 200/404.
```sql
INSERT INTO institutions (updated, institution_id, name, raw) VALUES (NOW, $1, $2, $3), (NOW, $4, $5, $6), …
ON CONFLICT (institution_id) DO UPDATE SET name = EXCLUDED.name, raw = EXCLUDED.raw, updated = NOW RETURNING *
```

**Q-INS-2 — batch lookup by ids (G4).** `repo/institutions.rs:57`.
```sql
SELECT * FROM institutions WHERE institution_id IN ($1, …, $k)
```

## 2.4 `securities`

**Q-SEC-1 — the securities behind a user's holdings (inner join, `DISTINCT` over whole rows).** `repo/securities.rs:22`. Soft-deleted holdings are deliberately included. `DISTINCT s.*` compares every column including the JSONB `raw`.
```sql
SELECT DISTINCT s.* FROM securities s
INNER JOIN holdings h ON h.security_id = s.security_id
WHERE h.user_id = ANY(sharing_user_ids($1))
```

**Q-SEC-2 — one security by id (G3).** `repo/securities.rs:33`. `search_securities_by_id` (`:71`) calls it once per id in a loop (N+1), and that loop is used by the snapshot stream, the hourly price refresh, the plan-statement import and `GET /holding-snapshots`.
```sql
SELECT * FROM securities WHERE security_id = $1 LIMIT $2
```

**Q-SEC-3 — search by optional exact fields, newest first.** `repo/securities.rs:64`. Callers take the first row, so `updated DESC` is the tie-break among rows sharing a ticker.
```sql
SELECT * FROM securities [WHERE [security_id = $1] [AND ticker_symbol = $2] [AND name = $3]] ORDER BY updated DESC
```

**Q-SEC-4 — upsert one security (G7, every supplied column).** `repo/securities.rs:89`, one statement per security.
```sql
INSERT INTO securities (updated, security_id, name, ticker_symbol, type, close_price, close_price_as_of, iso_currency_code, isin, cusip, raw)
VALUES (NOW, $1, …) ON CONFLICT (security_id) DO UPDATE SET name = EXCLUDED.name, …, raw = EXCLUDED.raw, updated = NOW RETURNING *
```

## 2.5 `items`

**Q-ITM-1 — one item in the caller's group (G3).** `repo/items.rs:21`.
```sql
SELECT * FROM items WHERE user_id = ANY(sharing_user_ids($1)) AND item_id = $2 AND LIVE LIMIT $3
```

**Q-ITM-2 — every live item of every user.** `repo/items.rs:26` (hourly sync).
```sql
SELECT * FROM items WHERE LIVE
```

**Q-ITM-3 — search by optional exact fields.** `repo/items.rs:51`.
```sql
SELECT * FROM items WHERE GROUP($1) [AND item_id = $2] [AND institution_id = $3] [AND provider = $4] AND LIVE
```

**Q-ITM-4 — an item with its owner's name (inner join).** `repo/items.rs:81`. The parameter is bound as whatever JSON value the webhook carried, so a number makes the statement fail with a type error, which the app expects.
```sql
SELECT i.*, u.username FROM items i JOIN users u ON i.user_id = u.user_id
WHERE i.item_id = $1 AND (i.is_deleted IS NULL OR i.is_deleted = FALSE)
```

**Q-ITM-5 — upsert with a column allowlist (G7).** `repo/items.rs:117`. On conflict only the allowlisted columns that were supplied are rewritten (`access_token`, `institution_id`, `available_products`, `cursor`, `status`, `status_reason`, `provider`, `last_sync_status`, `last_sync_at`, `last_sync_error`, `raw`), never `user_id`. The Plaid sync uses it with just `{item_id, cursor}` to advance the cursor (which also replaces `raw` with that partial object, since `raw` is on the allowlist).
```sql
INSERT INTO items (updated, item_id, user_id[, …supplied columns…], raw) VALUES (NOW, $1, …)
ON CONFLICT (item_id) DO UPDATE SET <allowlisted supplied column> = EXCLUDED.<column>, …, updated = NOW RETURNING *
```

**Q-ITM-6 — update by key within the group (G6).** `repo/items.rs:138`.
```sql
UPDATE items SET updated = NOW, status = $1, status_reason = $2 WHERE item_id = $3 AND GROUP($4) AND LIVE RETURNING item_id
```

**Q-ITM-7 — record a sync outcome (G6, no owner scope).** `repo/items.rs:154`.
```sql
UPDATE items SET updated = NOW, last_sync_status = $1, last_sync_at = $2, last_sync_error = $3 WHERE item_id = $4 AND LIVE RETURNING item_id
```

**Q-ITM-8 — soft-delete by key within the group (G10).** `repo/items.rs:199`, last statement of the delete-item transaction; its row count is the route's answer.
```sql
UPDATE items SET is_deleted = TRUE, updated = NOW WHERE item_id IN ($1) AND GROUP($2) RETURNING item_id
```

## 2.6 `accounts`

**Q-ACC-1 — all accounts of the group.** `repo/accounts.rs:20`.
```sql
SELECT * FROM accounts WHERE GROUP($1) AND LIVE
```

**Q-ACC-2 — one account (G3).** `repo/accounts.rs:25`.
```sql
SELECT * FROM accounts WHERE GROUP($1) AND account_id = $2 AND LIVE LIMIT $3
```

**Q-ACC-3 — filtered list.** `repo/accounts.rs:30` (by item), `:60` (any of the four), `repo/items.rs:161` (by item, filter order reversed).
```sql
SELECT * FROM accounts WHERE GROUP($1) [AND account_id = $2] [AND item_id = $3] [AND institution_id = $4] [AND type = $5] AND LIVE
```

**Q-ACC-4 — by ids within the group (G4).** `repo/accounts.rs:69`.
```sql
SELECT * FROM accounts WHERE account_id IN ($1, …, $k) AND LIVE AND user_id = ANY(sharing_user_ids($k+1))
```

**Q-ACC-5 — insert a manual account (G5).** `repo/accounts.rs:119`.
```sql
INSERT INTO accounts (updated, user_id, account_id, item_id, institution_id, name, type, subtype,
  balances_available, balances_current, balances_limit, balances_iso_currency_code[, graph_options_…])
VALUES (NOW, $1, …) RETURNING *
```

**Q-ACC-6 — upsert from a provider sync (G7, every supplied column, `user_id` included).** `repo/accounts.rs:143`, one statement per account.
```sql
INSERT INTO accounts (updated, user_id, account_id, …, raw) VALUES (NOW, $1, …)
ON CONFLICT (account_id) DO UPDATE SET user_id = EXCLUDED.user_id, …, raw = EXCLUDED.raw, updated = NOW RETURNING *
```

**Q-ACC-7 — edit an account (G6).** `repo/accounts.rs:182`. Returned row → 200, none → 304.
```sql
UPDATE accounts SET updated = NOW, <supplied columns except account_id, user_id, item_id, institution_id, raw> WHERE account_id = $n AND GROUP($n+1) AND LIVE RETURNING account_id
```

**Q-ACC-8 — store a JSONB object on the owner's own account, inside a transaction (G6).** `srv/plan_statement/import.rs:333`.
```sql
UPDATE accounts SET updated = NOW, plan_fund_mappings = $1 WHERE account_id = $2 AND user_id = $3 AND LIVE RETURNING account_id
```

**Q-ACC-9 — soft-delete by ids within the group (G10).** `repo/accounts.rs:231`, `repo/items.rs:196`.
```sql
UPDATE accounts SET is_deleted = TRUE, updated = NOW WHERE account_id IN ($1, …, $k) AND GROUP($k+1) RETURNING account_id
```

`accounts` is also joined by Q-TXN-12 and Q-TXN-14 (to exclude hidden accounts) and probed by Q-SHR-5.

## 2.7 `holdings`

**Q-HLD-1 — all holdings of the group.** `repo/holdings.rs:14`.
```sql
SELECT * FROM holdings WHERE GROUP($1) AND LIVE
```

**Q-HLD-2 — holdings of some accounts (`IN`).** `repo/holdings.rs:80`.
```sql
SELECT * FROM holdings WHERE GROUP($1) AND account_id IN ($2, …, $k) AND LIVE
```

**Q-HLD-3 — upsert with a column allowlist (G7).** `repo/holdings.rs:42`; `:58` inside the plan-statement transaction (must raise on failure). Allowlist: `security_id`, `institution_price`, `institution_price_as_of`, `institution_value`, `cost_basis`, `quantity`, `iso_currency_code`, `raw`; never `user_id` or `account_id`.
```sql
INSERT INTO holdings (updated, user_id, holding_id, account_id, security_id, …, raw) VALUES (NOW, $1, …)
ON CONFLICT (holding_id) DO UPDATE SET security_id = EXCLUDED.security_id, …, raw = EXCLUDED.raw, updated = NOW RETURNING *
```

**Q-HLD-4 — soft-delete by ids within the group (G10).** `repo/holdings.rs:72`.
```sql
UPDATE holdings SET is_deleted = TRUE, updated = NOW WHERE holding_id IN ($1, …, $k) AND GROUP($k+1) RETURNING holding_id
```

**Q-HLD-5 — soft-delete by account, one or many (G11).** `repo/accounts.rs:225` (scalar), `repo/items.rs:193` (array).
```sql
UPDATE holdings SET is_deleted = TRUE, updated = NOW WHERE account_id = $1 AND GROUP($2) RETURNING holding_id
UPDATE holdings SET is_deleted = TRUE, updated = NOW WHERE account_id = ANY($1) AND GROUP($2) RETURNING holding_id
```

**Q-HLD-6 — distinct securities held by anyone.** `ct/refresh_security_snapshots.rs:214` (hourly).
```sql
SELECT DISTINCT security_id FROM holdings WHERE LIVE AND security_id IS NOT NULL
```

## 2.8 `transactions`

Reads

**Q-TXN-1 — one transaction in the group (G3).** `repo/transactions.rs:103`.
```sql
SELECT * FROM transactions WHERE GROUP($1) AND transaction_id = $2 AND LIVE LIMIT $3
```

**Q-TXN-2 — keyset-paged stream, optionally a delta by `updated` (G2).** `repo/transactions.rs:154`; `GET /transactions` (`routes/accounts/get_transactions.rs:51`) always asks for tombstones, so `LIVE` is absent there. This is the app-load hot path: cold sync reads the whole table in 500-row pages, warm sync adds `updated >= <cursor>`.
```sql
SELECT * FROM transactions
WHERE GROUP($1) [AND account_id = $2] [AND pending = $3] [AND updated >= $4] [AND updated <= $5] [AND LIVE] [AND transaction_id > $6]
ORDER BY transaction_id LIMIT $7
```

**Q-TXN-3 — by ids within the group (G4).** `repo/transactions.rs:171` (*unused*), `routes/accounts/post_suggest_category.rs:229` (up to 500 ids).
```sql
SELECT * FROM transactions WHERE transaction_id IN ($1, …, $k) AND LIVE AND user_id = ANY(sharing_user_ids($k+1))
```

**Q-TXN-4 — by ids through the filter builder.** `repo/transfers.rs:228` (both halves of every visible pair), `repo/recurrings.rs:217` (every member of every visible recurring), `routes/recurrings/post_recurring.rs:228` (strict `user_id = $1`). The `IN` list has one bind parameter per id and can be thousands long.
```sql
SELECT * FROM transactions WHERE GROUP($1) AND transaction_id IN ($2, …, $k) AND LIVE
SELECT * FROM transactions WHERE user_id = $1 AND transaction_id IN ($2, …, $k) AND LIVE
```

**Q-TXN-5 — by accounts and a `date` range, newest first.** `repo/transactions.rs:457` (provider sync reads the stored window, tombstones included). Date bounds are `YYYY-MM-DD` text compared with a `DATE` column.
```sql
SELECT * FROM transactions WHERE GROUP($1) AND account_id IN ($2, …, $k) [AND date >= $k+1] [AND date <= $k+2] [AND LIVE] ORDER BY date DESC
```

**Q-TXN-6 — every unlabeled transaction of one user (`IS NULL` filter).** `ct/auto_suggest.rs:216`. Returns whole rows (with `raw`); the 7-day window is applied in the app.
```sql
SELECT * FROM transactions WHERE user_id = $1 AND label_category_confidence IS NULL AND LIVE
```

**Q-TXN-7 — highest `Unknown_N` name (regex match, regex capture, cast, `MAX`).** `repo/transactions.rs:265`. Soft-deleted rows count.
```sql
SELECT COALESCE(MAX(CAST(SUBSTRING(name FROM '^Unknown_(\d+)$') AS INTEGER)), 0) AS max
FROM transactions WHERE user_id = $1 AND name ~ '^Unknown_[0-9]+$'
```

**Q-TXN-8 — current label of one transaction (narrow projection).** `srv/record_category_rejection.rs:88`, before every `POST /transaction`.
```sql
SELECT label_category_id, label_budget_id, label_category_confidence FROM transactions
WHERE transaction_id = $1 AND user_id = ANY(sharing_user_ids($2)) AND LIVE LIMIT 1
```

**Q-TXN-9 — existence check with a shared row lock.** `repo/transfers.rs:315` (group scope, returns owners), `repo/recurrings.rs:794` (own rows), `repo/recurrings.rs:944` (own, settled rows, with amounts). The caller compares the number of rows with the number of ids asked for.
```sql
SELECT transaction_id, user_id FROM transactions
WHERE user_id = ANY(sharing_user_ids($1)) AND transaction_id = ANY($2::text[]) AND LIVE FOR SHARE

SELECT transaction_id FROM transactions WHERE user_id = $1 AND transaction_id = ANY($2::text[]) AND LIVE FOR SHARE

SELECT transaction_id, amount FROM transactions
WHERE user_id = $1 AND transaction_id = ANY($2::text[]) AND LIVE AND pending = FALSE FOR SHARE
```

**Q-TXN-10 — most common confirmed label among some transactions (`GROUP BY`, order by `COUNT` then `MAX`, `LIMIT 1`).** `repo/recurrings.rs:345`.
```sql
SELECT label_budget_id AS budget_id, label_category_id AS category_id FROM transactions
WHERE user_id = $1 AND transaction_id = ANY($2::text[]) AND LIVE
  AND label_category_confidence = 1 AND label_category_id IS NOT NULL
GROUP BY label_budget_id, label_category_id
ORDER BY COUNT(*) DESC, MAX(date) DESC
LIMIT 1
```

**Q-TXN-11 — auto-suggest feature signal (two CTEs, per-row weighted score, trigram similarity, JSON path, aggregate, correlated scalar subquery with a join, two more joins).** `ct/auto_suggest.rs:469`, score expression `:445`. Run once or twice per unlabeled target, each run scanning all of the user's confirmed transactions. `{score}` appears three times.
```sql
WITH scored AS (
  SELECT t.label_category_id, {score} AS score
  FROM transactions t
  WHERE t.user_id = $1 AND t.label_category_confidence = 1.0 AND (t.is_deleted IS NULL OR t.is_deleted = FALSE)
),
winning AS (
  SELECT label_category_id, SUM(score)::int AS accepted, COUNT(*)::int AS count_matched
  FROM scored WHERE score > $12
  GROUP BY label_category_id ORDER BY accepted DESC LIMIT 1
)
SELECT w.label_category_id, sections.budget_id AS label_budget_id, w.accepted, w.count_matched,
  ( SELECT COALESCE(SUM({score})::int, 0)
    FROM rejected_categories rc
    JOIN transactions t ON t.transaction_id = rc.transaction_id AND t.user_id = rc.user_id
    WHERE rc.user_id = $1 AND rc.category_id = w.label_category_id
      AND (t.is_deleted IS NULL OR t.is_deleted = FALSE) AND {score} > $12
  ) AS rejected
FROM winning w
JOIN categories ON categories.category_id = w.label_category_id AND categories.user_id = ANY(sharing_user_ids($1))
JOIN sections   ON sections.section_id = categories.section_id   AND sections.user_id = ANY(sharing_user_ids($1))
LIMIT 1

-- {score} for row t:
(
  (CASE WHEN $2::text IS NOT NULL AND t.merchant_name IS NOT NULL
        AND similarity(t.merchant_name, $2) >= $3
        AND (SIGN($13::numeric) = 0 OR SIGN(t.amount) = SIGN($13::numeric)) THEN 100 ELSE 0 END)
+ (CASE WHEN $4::text IS NOT NULL AND t.name IS NOT NULL
        AND similarity(t.name, $4) >= $3
        AND (SIGN($13::numeric) = 0 OR SIGN(t.amount) = SIGN($13::numeric)) THEN 50 ELSE 0 END)
+ (CASE WHEN t.amount BETWEEN $5 AND $6 THEN 5 ELSE 0 END)
+ (CASE WHEN $7::text IS NOT NULL AND t.payment_channel = $7 THEN 1 ELSE 0 END)
+ (CASE WHEN t.account_id = $8 THEN 1 ELSE 0 END)
+ (CASE WHEN $9::text IS NOT NULL
        AND (t.raw->'personal_finance_category'->>'primary') = $9
        AND (SIGN($13::numeric) = 0 OR SIGN(t.amount) = SIGN($13::numeric)) THEN 10 ELSE 0 END)
+ (CASE WHEN EXTRACT(DAY FROM t.date::date) BETWEEN $10 AND $11 THEN 1 ELSE 0 END)
)
```
Parameters: `$1` user, `$2` merchant name or null, `$3` 0.5, `$4` name or null, `$5`/`$6` amount band (float8), `$7` channel or null, `$8` account, `$9` Plaid category or null, `$10`/`$11` day-of-month band (may be below 1 or above 31), `$12` score floor (19, then 15), `$13` target amount. A tie on `accepted` is broken arbitrarily.

**Q-TXN-12 — recurring-detection candidates (inner join, `COALESCE`, anti-join with `OR`).** `ct/detect_recurrings.rs:596`, hourly per user: all settled, non-zero transactions on visible accounts that are not half of a confirmed transfer.
```sql
SELECT t.transaction_id, t.amount, t.date::text AS date, t.merchant_name, t.name
FROM transactions t
JOIN accounts a ON a.account_id = t.account_id
WHERE t.user_id = $1 AND (t.is_deleted IS NULL OR t.is_deleted = FALSE)
  AND COALESCE(t.pending, FALSE) = FALSE AND t.amount <> 0 AND COALESCE(a.hide, FALSE) = FALSE
  AND NOT EXISTS (
    SELECT 1 FROM transaction_pairs tp
    WHERE tp.user_id = t.user_id AND (tp.is_deleted IS NULL OR tp.is_deleted = FALSE) AND tp.status = 'confirmed'
      AND (tp.transaction_id_a = t.transaction_id OR tp.transaction_id_b = t.transaction_id))
ORDER BY t.transaction_id
```
Note the join to `accounts` has no owner or soft-delete condition.

**Q-TXN-13 — members of confirmed recurrings (`= ANY` array parameter).** `ct/detect_recurrings.rs:641`.
```sql
SELECT t.transaction_id, t.amount, t.date::text AS date, t.merchant_name
FROM transactions t WHERE t.user_id = $1 AND t.transaction_id = ANY($2::text[]) AND (t.is_deleted IS NULL OR t.is_deleted = FALSE)
```

**Q-TXN-14 — transfer-detection candidates (self-join on a negated amount and a date window, two more joins, two anti-joins, computed columns, `ORDER BY` an expression, `LIMIT`).** `ct/detect_transfers.rs:73`, hourly per sharing group. `$1` is the group's user ids as `uuid[]`, `$2` = 7 days, `$3` = 500 × members.
```sql
SELECT t1.transaction_id AS transaction_id_a, t2.transaction_id AS transaction_id_b,
       t1.user_id AS user_id_a, t2.user_id AS user_id_b,
       ABS(t1.date - t2.date) AS date_delta,
       ( (t1.raw->'personal_finance_category'->>'primary') LIKE 'TRANSFER%'
         OR (t2.raw->'personal_finance_category'->>'primary') LIKE 'TRANSFER%' ) AS is_plaid_transfer
FROM transactions t1
JOIN transactions t2
  ON t1.user_id = ANY($1::uuid[]) AND t2.user_id = ANY($1::uuid[])
 AND t2.amount = -t1.amount AND t1.amount <> 0
 AND t1.account_id <> t2.account_id
 AND t2.date BETWEEN t1.date - $2::int AND t1.date + $2::int
 AND t1.transaction_id < t2.transaction_id
JOIN accounts a1 ON a1.account_id = t1.account_id AND a1.user_id = t1.user_id
JOIN accounts a2 ON a2.account_id = t2.account_id AND a2.user_id = t2.user_id
WHERE (t1.is_deleted IS NULL OR t1.is_deleted = FALSE) AND (t2.is_deleted IS NULL OR t2.is_deleted = FALSE)
  AND COALESCE(a1.hide, FALSE) = FALSE AND COALESCE(a2.hide, FALSE) = FALSE
  AND NOT EXISTS (
    SELECT 1 FROM transaction_pairs tp
    WHERE tp.user_id = ANY($1::uuid[]) AND (tp.is_deleted IS NULL OR tp.is_deleted = FALSE) AND tp.status <> 'rejected'
      AND (tp.transaction_id_a IN (t1.transaction_id, t2.transaction_id)
           OR tp.transaction_id_b IN (t1.transaction_id, t2.transaction_id)))
  AND NOT EXISTS (
    SELECT 1 FROM transaction_pairs tp
    WHERE tp.user_id = ANY($1::uuid[]) AND (tp.is_deleted IS NULL OR tp.is_deleted = FALSE) AND tp.status = 'rejected'
      AND ((tp.transaction_id_a = t1.transaction_id AND tp.transaction_id_b = t2.transaction_id)
        OR (tp.transaction_id_a = t2.transaction_id AND tp.transaction_id_b = t1.transaction_id)))
ORDER BY ABS(t1.date - t2.date) ASC, t1.transaction_id ASC
LIMIT $3
```
`date - date` is an integer number of days, `date - int` is a date, `t2.amount = -t1.amount` is exact decimal equality.

Writes

**Q-TXN-15 — upsert from a provider sync (G7, every supplied column).** `repo/transactions.rs:181`, one statement per transaction; label columns are in the statement only when the sync supplies them.
```sql
INSERT INTO transactions (updated, user_id, transaction_id, account_id, name, merchant_name, amount, iso_currency_code, date, pending,
  pending_transaction_id, payment_channel, location_country, location_region, location_city[, label_…][, source], raw)
VALUES (NOW, $1, …)
ON CONFLICT (transaction_id) DO UPDATE SET user_id = EXCLUDED.user_id, …, raw = EXCLUDED.raw, updated = NOW RETURNING *
```

**Q-TXN-16 — insert a manual transaction (G5).** `repo/transactions.rs:305`.
```sql
INSERT INTO transactions (updated, user_id, transaction_id, account_id, name, amount, iso_currency_code, date, pending, source, raw)
VALUES (NOW, $1, …) RETURNING *
```

**Q-TXN-17 — edit a transaction within the group (G6).** `repo/transactions.rs:241` (200 / 304 from the returned row), `routes/accounts/post_suggest_category.rs:173` ("no row" is reported as a concurrent modification).
```sql
UPDATE transactions SET updated = NOW, <supplied columns except transaction_id, user_id, account_id, raw> WHERE transaction_id = $n AND GROUP($n+1) AND LIVE RETURNING transaction_id
```

**Q-TXN-18 — compare-and-swap label write (G6 with `IS NULL` guard).** `ct/auto_suggest.rs:249`. The engine may only label a row that is still unlabeled when the statement runs.
```sql
UPDATE transactions SET updated = NOW, label_category_id = $1, label_budget_id = $2, label_category_confidence = $3
WHERE transaction_id = $4 AND user_id = $5 AND label_category_confidence IS NULL AND LIVE RETURNING transaction_id
```

**Q-TXN-19 — propagate a recurring's default label (set-based `UPDATE`, array parameter, NULL-safe comparisons, four-way `OR`, no-op suppression).** `repo/recurrings.rs:297`. The last `AND (…)` skips rows that would not change, so their `updated` is not bumped.
```sql
UPDATE transactions
SET label_budget_id = $3::uuid, label_category_id = $4::uuid, label_category_confidence = $5::float, updated = NOW
WHERE user_id = $1 AND transaction_id = ANY($2::text[]) AND LIVE
  AND ( label_category_confidence IS NULL
     OR (label_category_confidence > 0 AND label_category_confidence < 1)
     OR (label_category_confidence >= 1
         AND label_budget_id IS NOT DISTINCT FROM $6::uuid AND label_category_id IS NOT DISTINCT FROM $7::uuid)
     OR (label_category_confidence <= 0 AND (label_budget_id IS NULL OR label_budget_id = $6::uuid)) )
  AND ( label_budget_id IS DISTINCT FROM $3::uuid
     OR label_category_id IS DISTINCT FROM $4::uuid
     OR label_category_confidence IS DISTINCT FROM $5::float )
```

**Q-TXN-20 — soft-delete by ids within the group (G10).** `repo/transactions.rs:376`. Its row count is the delete route's answer and the sync's "removed" count.
```sql
UPDATE transactions SET is_deleted = TRUE, updated = NOW WHERE transaction_id IN ($1, …, $k) AND GROUP($k+1) RETURNING transaction_id
```

**Q-TXN-21 — soft-delete by account, one or many (G11).** `repo/accounts.rs:217`, `repo/items.rs:184`.
```sql
UPDATE transactions SET is_deleted = TRUE, updated = NOW WHERE account_id = $1 AND GROUP($2) RETURNING transaction_id
UPDATE transactions SET is_deleted = TRUE, updated = NOW WHERE account_id = ANY($1) AND GROUP($2) RETURNING transaction_id
```

`transactions` is also read inside Q-PAR-8, Q-PAR-9, Q-PAR-11, Q-REC-12, Q-SPL-8, Q-SPL-9, Q-REJ-1 and Q-SHR-5.

## 2.9 `transaction_pairs`

**Q-PAR-1 — pairs of the group, optionally a delta, ordered by key.** `repo/transfers.rs:188`. Rejected pairs are filtered out in the app (the builder has no `<>`); with tombstones requested they are returned as eviction signals.
```sql
SELECT * FROM transaction_pairs WHERE GROUP($1) [AND updated >= $2] [AND LIVE] ORDER BY pair_id
```

**Q-PAR-2 — collision probe: is either transaction in another confirmed pair?** `repo/transfers.rs:344` (manual pairing), `:505` (confirm). Only whether a row came back matters.
```sql
SELECT pair_id FROM transaction_pairs
WHERE GROUP($1) AND LIVE AND status = 'confirmed'
  AND (transaction_id_a = $2 OR transaction_id_b = $2 OR transaction_id_a = $3 OR transaction_id_b = $3)
  AND NOT (transaction_id_a = $4 AND transaction_id_b = $5)
LIMIT 1

SELECT pair_id FROM transaction_pairs
WHERE GROUP($1) AND pair_id <> $2 AND LIVE AND status = 'confirmed'
  AND (transaction_id_a = $3 OR transaction_id_b = $3 OR transaction_id_a = $4 OR transaction_id_b = $4)
LIMIT 1
```

**Q-PAR-3 — upsert on the unique pair `(transaction_id_a, transaction_id_b)`, with `CASE` over the existing row.** `repo/transfers.rs:371`. A new pair is inserted; a soft-deleted or rejected one is revived with the new status and a fresh `updated`; a live suggested/confirmed one keeps its status and its `updated` (so no delta event). `RETURNING` gives the id of whichever row now stands, which may be an older `pair_id` than the one offered.
```sql
INSERT INTO transaction_pairs (pair_id, user_id, transaction_id_a, transaction_id_b, status)
VALUES ($1, $2, $3, $4, $5)
ON CONFLICT (transaction_id_a, transaction_id_b) DO UPDATE SET
  status = CASE WHEN transaction_pairs.is_deleted = TRUE THEN EXCLUDED.status
                WHEN transaction_pairs.status = 'rejected' THEN EXCLUDED.status
                ELSE transaction_pairs.status END,
  is_deleted = FALSE,
  updated = CASE WHEN transaction_pairs.is_deleted = TRUE OR transaction_pairs.status = 'rejected'
                 THEN CURRENT_TIMESTAMP ELSE transaction_pairs.updated END
RETURNING pair_id
```

**Q-PAR-4 — reject every other suggested pair touching two transactions (set-based conditional `UPDATE`).** `repo/transfers.rs:420`, `:547`.
```sql
UPDATE transaction_pairs SET status = 'rejected', updated = NOW
WHERE GROUP($1) AND LIVE AND status = 'suggested' AND pair_id <> $2
  AND (transaction_id_a = $3 OR transaction_id_b = $3 OR transaction_id_a = $4 OR transaction_id_b = $4)
```

**Q-PAR-5 — read a pair and lock it for update.** `repo/transfers.rs:482`.
```sql
SELECT transaction_id_a, transaction_id_b FROM transaction_pairs
WHERE pair_id = $1 AND GROUP($2) AND LIVE AND status <> 'rejected'
FOR UPDATE
```

**Q-PAR-6 — set one pair's status.** `repo/transfers.rs:526` (`'confirmed'`, inside the confirm transaction), `:585` (`'rejected'`, a lone autocommit statement whose result is ignored).
```sql
UPDATE transaction_pairs SET status = 'confirmed', updated = NOW WHERE pair_id = $1 AND GROUP($2) AND LIVE
UPDATE transaction_pairs SET status = 'rejected',  updated = NOW WHERE pair_id = $1 AND GROUP($2) AND LIVE
```

**Q-PAR-7 — soft-delete pairs naming any of some transactions (G11, array parameter, once per side).** `repo/transactions.rs:373`, `:374`.
```sql
UPDATE transaction_pairs SET is_deleted = TRUE, updated = NOW WHERE transaction_id_a = ANY($1) AND GROUP($2) RETURNING pair_id
UPDATE transaction_pairs SET is_deleted = TRUE, updated = NOW WHERE transaction_id_b = ANY($1) AND GROUP($2) RETURNING pair_id
```

**Q-PAR-8 — soft-delete pairs whose transactions sit on some accounts (G12, `IN (subquery)`, once per side).** `repo/transactions.rs:342`. The subquery deliberately has no soft-delete filter: it runs right after those transactions were soft-deleted in the same transaction.
```sql
UPDATE transaction_pairs SET is_deleted = TRUE, updated = NOW
WHERE transaction_id_a IN (SELECT transaction_id FROM transactions WHERE account_id = ANY($1))
  AND user_id = ANY(sharing_user_ids($2)) RETURNING pair_id
-- and the same with transaction_id_b
```

**Q-PAR-9 — soft-delete stale pairs (correlated `EXISTS`).** `ct/detect_transfers.rs:232`. The affected-row count is logged.
```sql
UPDATE transaction_pairs tp SET is_deleted = TRUE, updated = NOW
WHERE tp.user_id = ANY($1::uuid[]) AND (tp.is_deleted IS NULL OR tp.is_deleted = FALSE)
  AND EXISTS (SELECT 1 FROM transactions t
              WHERE t.transaction_id IN (tp.transaction_id_a, tp.transaction_id_b) AND t.is_deleted = TRUE)
```

**Q-PAR-10 — insert a suggestion unless either transaction is already paired (`INSERT … SELECT … WHERE NOT EXISTS`, two anti-joins).** `ct/detect_transfers.rs:167`. A conflicting row of a user outside the bound group still trips the unique constraint; that error is caught by the savepoint around this statement (section 3). The affected-row count is **not** checked: the job counts a suggestion even when the anti-joins made the statement insert nothing (a behaviour `conformance/README.md` pins).
```sql
INSERT INTO transaction_pairs (pair_id, user_id, transaction_id_a, transaction_id_b, status)
SELECT $1, $2, $3, $4, 'suggested'
WHERE NOT EXISTS (
        SELECT 1 FROM transaction_pairs tp
        WHERE tp.user_id = ANY($5::uuid[]) AND (tp.is_deleted IS NULL OR tp.is_deleted = FALSE) AND tp.status <> 'rejected'
          AND (tp.transaction_id_a IN ($3, $4) OR tp.transaction_id_b IN ($3, $4)))
  AND NOT EXISTS (
        SELECT 1 FROM transaction_pairs tp
        WHERE tp.user_id = ANY($5::uuid[])
          AND ((tp.transaction_id_a = $3 AND tp.transaction_id_b = $4) OR (tp.transaction_id_a = $4 AND tp.transaction_id_b = $3)))
```

**Q-PAR-11 — soft-delete pairs split by a sharing break-up (`UPDATE … FROM` two aliases of another table, boolean inequality).** `repo/sharing.rs:506`. `$1` is the former group as `uuid[]`, `$2` the leaving member.
```sql
UPDATE transaction_pairs pair SET is_deleted = TRUE, updated = NOW
FROM transactions a, transactions b
WHERE a.transaction_id = pair.transaction_id_a AND b.transaction_id = pair.transaction_id_b
  AND pair.user_id = ANY($1::uuid[]) AND (pair.is_deleted IS NULL OR pair.is_deleted = FALSE)
  AND (a.user_id = $2) <> (b.user_id = $2)
```

`transaction_pairs` is also the anti-join target in Q-TXN-12 and Q-TXN-14.

## 2.10 `recurrings`

**Q-REC-1 — recurrings of the group, optionally some ids, optionally a delta, ordered by key.** `repo/recurrings.rs:179`. Dismissed rows are filtered in the app.
```sql
SELECT * FROM recurrings WHERE GROUP($1) [AND recurring_id IN ($2, …)] [AND updated >= $k] [AND LIVE] ORDER BY recurring_id
```

**Q-REC-2 — every row of one user, tombstones included.** `ct/detect_recurrings.rs:726` (inside the detection transaction).
```sql
SELECT * FROM recurrings WHERE user_id = $1
```

**Q-REC-3 — read one confirmed recurring's defaults.** `repo/recurrings.rs:499`. No soft-delete filter here.
```sql
SELECT label_budget_id, label_category_id, transaction_ids, splits, splits_since FROM recurrings
WHERE user_id = $1 AND recurring_id = $2 AND status = 'confirmed'
```

**Q-REC-4 — read one live recurring of the user (narrow projections).** `repo/recurrings.rs:609`, `:923`.
```sql
SELECT label_budget_id, label_category_id, status, transaction_ids FROM recurrings WHERE user_id = $1 AND recurring_id = $2 AND LIVE
SELECT recurring_id, is_income, status FROM recurrings WHERE user_id = $1 AND recurring_id = $2 AND LIVE
```

**Q-REC-5 — set the default label.** `repo/recurrings.rs:523`, `:624`.
```sql
UPDATE recurrings SET label_budget_id = $3::uuid, label_category_id = $4::uuid, updated = NOW WHERE user_id = $1 AND recurring_id = $2
```

**Q-REC-6 — confirm, and learn whether the row existed.** `repo/recurrings.rs:567`. Zero rows → roll back and answer "not found".
```sql
UPDATE recurrings SET status = 'confirmed', updated = NOW WHERE user_id = $1 AND recurring_id = $2 AND LIVE RETURNING recurring_id
```

**Q-REC-7 — builder updates of the user's own row (G6).** `repo/recurrings.rs:684` (JSONB `splits` plus a `DATE`), `:707` (`status = 'dismissed'` or `merchant_name`). "No row" → "Recurring not found."
```sql
UPDATE recurrings SET updated = NOW, splits = $1, splits_since = $2 WHERE recurring_id = $3 AND user_id = $4 AND LIVE RETURNING recurring_id
UPDATE recurrings SET updated = NOW, status = $1 WHERE recurring_id = $2 AND user_id = $3 AND LIVE RETURNING recurring_id
```

**Q-REC-8 — collision probe with array overlap.** `repo/recurrings.rs:816`, `:970` (the second excludes the recurring being added to).
```sql
SELECT recurring_id FROM recurrings
WHERE user_id = $1 AND LIVE AND status <> 'dismissed' [AND recurring_id <> $2] AND transaction_ids && $3::text[]
LIMIT 1
```

**Q-REC-9 — upsert on the unique `(user_id, merchant_key, is_income)`: confirm, conditionally rename, union the member arrays, undelete.** `repo/recurrings.rs:834`. `$8` is a boolean parameter deciding inside the statement whether the name is overwritten. `RETURNING` gives the surviving row's id.
```sql
INSERT INTO recurrings (recurring_id, user_id, merchant_key, merchant_name, cadence, is_income, status, transaction_ids)
VALUES ($1, $2, $3, $4, $5, $6, 'confirmed', $7)
ON CONFLICT (user_id, merchant_key, is_income) DO UPDATE SET
  status = 'confirmed',
  merchant_name = CASE WHEN $8::boolean THEN EXCLUDED.merchant_name ELSE recurrings.merchant_name END,
  transaction_ids = (SELECT ARRAY(SELECT DISTINCT UNNEST(recurrings.transaction_ids || EXCLUDED.transaction_ids))),
  is_deleted = FALSE,
  updated = CURRENT_TIMESTAMP
RETURNING recurring_id
```

**Q-REC-10 — add members, de-duplicated (array concatenation, `DISTINCT UNNEST`).** `repo/recurrings.rs:989`. Element order after the union is whatever the server produces.
```sql
UPDATE recurrings SET transaction_ids = (SELECT ARRAY(SELECT DISTINCT UNNEST(transaction_ids || $2::text[]))), updated = NOW
WHERE recurring_id = $1 AND user_id = $3
```

**Q-REC-11 — append members (`x = x || $n`).** `ct/detect_recurrings.rs:855`.
```sql
UPDATE recurrings SET transaction_ids = transaction_ids || $2::text[], updated = NOW WHERE recurring_id = $1
```

**Q-REC-12 — soft-delete recurrings none of whose members is live (correlated `NOT EXISTS` over `= ANY(array column)`).** `ct/detect_recurrings.rs:701`. Row count is logged.
```sql
UPDATE recurrings r SET is_deleted = TRUE, updated = NOW
WHERE r.user_id = $1 AND (r.is_deleted IS NULL OR r.is_deleted = FALSE)
  AND NOT EXISTS (SELECT 1 FROM transactions t
                  WHERE t.user_id = $1 AND t.transaction_id = ANY(r.transaction_ids)
                    AND (t.is_deleted IS NULL OR t.is_deleted = FALSE))
```

**Q-REC-13 — insert a suggestion, or revive a tombstone only (`ON CONFLICT … DO UPDATE … WHERE`).** `ct/detect_recurrings.rs:756`. When the conflicting row is live the `WHERE` is false, nothing is written **and nothing is returned**; when it is soft-deleted it is revived keeping its old status, which the caller reads from `RETURNING` to decide what to do next.
```sql
INSERT INTO recurrings (recurring_id, user_id, merchant_key, merchant_name, cadence, is_income, status, transaction_ids)
VALUES ($1, $2, $3, $4, $5, $6, 'suggested', $7)
ON CONFLICT (user_id, merchant_key, is_income) DO UPDATE SET
  cadence = EXCLUDED.cadence, transaction_ids = EXCLUDED.transaction_ids, is_deleted = FALSE, updated = CURRENT_TIMESTAMP
WHERE recurrings.is_deleted = TRUE
RETURNING recurring_id, status
```

**Q-REC-14 — replace the member list of a suggestion.** `ct/detect_recurrings.rs:813`. Issued only when the set actually changed, to avoid bumping `updated` for nothing.
```sql
UPDATE recurrings SET transaction_ids = $2, cadence = $3, updated = NOW WHERE recurring_id = $1
```

## 2.11 `investment_transactions`

**Q-INV-1 — one row in the group (G3).** `repo/investment_transactions.rs:55`.
```sql
SELECT * FROM investment_transactions WHERE GROUP($1) AND investment_transaction_id = $2 AND LIVE LIMIT $3
```

**Q-INV-2 — keyset-paged stream, optionally a delta (G2).** `repo/transactions.rs:156`; second half of `GET /transactions`.
```sql
SELECT * FROM investment_transactions
WHERE GROUP($1) [AND account_id = $2] [AND updated >= $3] [AND updated <= $4] [AND LIVE] [AND investment_transaction_id > $5]
ORDER BY investment_transaction_id LIMIT $6
```

**Q-INV-3 — by accounts and a `date` range, newest first.** `repo/transactions.rs:458`, issued concurrently with Q-TXN-5 on a second connection.
```sql
SELECT * FROM investment_transactions WHERE GROUP($1) AND account_id IN ($2, …) [AND date >= $k] [AND date <= $k+1] [AND LIVE] ORDER BY date DESC
```

**Q-INV-4 — list with `LIMIT`/`OFFSET`.** `repo/investment_transactions.rs:38`. *Unused* in both servers; the only offset pagination in the code base.
```sql
SELECT * FROM investment_transactions WHERE GROUP($1) [AND account_id = $2] [AND date >= $3] [AND date <= $4] AND LIVE ORDER BY date DESC [LIMIT $5] [OFFSET $6]
```

**Q-INV-5 — which of some ids exist for an owner, deleted ones included.** `srv/plan_statement/import.rs:104`.
```sql
SELECT * FROM investment_transactions WHERE user_id = $1 AND investment_transaction_id IN ($2, …, $k)
```

**Q-INV-6 — highest `Unknown_N` name.** `repo/investment_transactions.rs:142`. Same shape as Q-TXN-7 on this table.
```sql
SELECT COALESCE(MAX(CAST(SUBSTRING(name FROM '^Unknown_(\d+)$') AS INTEGER)), 0) AS max
FROM investment_transactions WHERE user_id = $1 AND name ~ '^Unknown_[0-9]+$'
```

**Q-INV-7 — upsert from a provider sync (G7, every supplied column).** `repo/investment_transactions.rs:68`.
```sql
INSERT INTO investment_transactions (updated, user_id, investment_transaction_id, account_id, security_id, date, name, amount, quantity, price, iso_currency_code, type, subtype[, label_…][, source], raw)
VALUES (NOW, $1, …) ON CONFLICT (investment_transaction_id) DO UPDATE SET user_id = EXCLUDED.user_id, …, updated = NOW RETURNING *
```

**Q-INV-8 — bulk insert that never overwrites (G8 with an empty update list, chunks of 1000 rows).** `srv/plan_statement/import.rs:329`. A row whose id already exists, even under another owner, is skipped silently.
```sql
INSERT INTO investment_transactions (updated, <columns>) VALUES (NOW, $1, …), (NOW, …), …
ON CONFLICT (investment_transaction_id) DO NOTHING RETURNING *
```

**Q-INV-9 — insert a manual row (G5).** `repo/investment_transactions.rs:224`.
```sql
INSERT INTO investment_transactions (updated, user_id, investment_transaction_id, account_id, security_id, date, name, amount, quantity, price, iso_currency_code, type, subtype, source, raw)
VALUES (NOW, $1, …) RETURNING *
```

**Q-INV-10 — edit within the group (G6).** `repo/investment_transactions.rs:116`.
```sql
UPDATE investment_transactions SET updated = NOW, <supplied columns except id, user_id, source, raw> WHERE investment_transaction_id = $n AND GROUP($n+1) AND LIVE RETURNING investment_transaction_id
```

**Q-INV-11 — soft-delete by ids within the group (G10).** `repo/investment_transactions.rs:249`.
```sql
UPDATE investment_transactions SET is_deleted = TRUE, updated = NOW WHERE investment_transaction_id IN ($1, …) AND GROUP($k) RETURNING investment_transaction_id
```

**Q-INV-12 — soft-delete by account, one or many (G11).** `repo/accounts.rs:218`, `repo/items.rs:185`.
```sql
UPDATE investment_transactions SET is_deleted = TRUE, updated = NOW WHERE account_id = $1 | account_id = ANY($1) AND GROUP($2) RETURNING investment_transaction_id
```

## 2.12 `split_transactions`

**Q-SPL-1 — splits of the group by optional exact fields, optionally a delta, newest first.** `repo/split_transactions.rs:32` (by parent), `:62` (`GET /split-transactions`, tombstones always included).
```sql
SELECT * FROM split_transactions WHERE GROUP($1) [AND transaction_id = $2] [AND account_id = $3] [AND updated >= $4] [AND updated <= $5] [AND LIVE] [ORDER BY date DESC]
```

**Q-SPL-2 — edit within the group (G6).** `repo/split_transactions.rs:98`.
```sql
UPDATE split_transactions SET updated = NOW, <supplied columns except id, user_id> WHERE split_transaction_id = $n AND GROUP($n+1) AND LIVE RETURNING split_transaction_id
```

**Q-SPL-3 — insert, id from the server (G5).** `repo/split_transactions.rs:251`.
```sql
INSERT INTO split_transactions (updated, user_id, transaction_id, account_id, amount, date, custom_name) VALUES (NOW, $1, $2, $3, $4, $5, $6) RETURNING *
```

**Q-SPL-4 — soft-delete by ids within the group (G10).** `repo/split_transactions.rs:130`.
```sql
UPDATE split_transactions SET is_deleted = TRUE, updated = NOW WHERE split_transaction_id IN ($1, …) AND GROUP($k) RETURNING split_transaction_id
```

**Q-SPL-5 — soft-delete by parent transactions or by account (G11).** `repo/transactions.rs:365` (`transaction_id = ANY($1)`), `repo/accounts.rs:219`, `repo/items.rs:186` (`account_id`).
```sql
UPDATE split_transactions SET is_deleted = TRUE, updated = NOW WHERE transaction_id = ANY($1) AND GROUP($2) RETURNING split_transaction_id
```

**Q-SPL-6 — "does this transaction have a live split?" (existence probe).** `repo/split_transactions.rs:197`.
```sql
SELECT 1 FROM split_transactions WHERE user_id = $1 AND transaction_id = $2 AND LIVE LIMIT 1
```

**Q-SPL-7 — re-point live splits from one parent to another (set-based `UPDATE` of a non-key column).** `repo/split_transactions.rs:217`. Affected-row count is returned.
```sql
UPDATE split_transactions SET transaction_id = $3, updated = NOW WHERE user_id = $1 AND transaction_id = $2 AND LIVE
```

**Q-SPL-8 — unlabeled splits with their parent's features (inner join on two columns, server-clock window).** `ct/auto_suggest.rs:255`.
```sql
SELECT st.split_transaction_id, t.merchant_name, t.name, t.amount, t.payment_channel, t.account_id, t.raw, t.date
FROM split_transactions st
JOIN transactions t ON t.transaction_id = st.transaction_id AND t.user_id = st.user_id
WHERE st.user_id = $1 AND st.label_category_confidence IS NULL
  AND (st.is_deleted IS NULL OR st.is_deleted = FALSE) AND (t.is_deleted IS NULL OR t.is_deleted = FALSE)
  AND t.date > NOW() - INTERVAL '1 week'
```

**Q-SPL-9 — create default splits for several transactions at once (`INSERT … SELECT`, `CROSS JOIN UNNEST` of three parallel arrays, arithmetic, `CASE`, anti-join).** `repo/recurrings.rs:441`. Produces (transactions × splits) rows in one statement; every produced row gets its own generated UUID, `updated` and `is_deleted` from column defaults.
```sql
INSERT INTO split_transactions (user_id, transaction_id, account_id, amount, date, label_budget_id, label_category_id, label_category_confidence)
SELECT t.user_id, t.transaction_id, t.account_id, s.amount * SIGN(t.amount), t.date,
       s.budget_id, s.category_id,
       CASE WHEN s.category_id IS NOT NULL THEN 1 WHEN s.budget_id IS NOT NULL THEN 0 END
FROM transactions t
CROSS JOIN UNNEST($3::numeric[], $4::uuid[], $5::uuid[]) AS s(amount, budget_id, category_id)
WHERE t.user_id = $1 AND t.transaction_id = ANY($2::text[]) AND (t.is_deleted IS NULL OR t.is_deleted = FALSE)
  AND ABS(t.amount) > $6::numeric AND t.date >= $7::date
  AND NOT EXISTS (SELECT 1 FROM split_transactions x
                  WHERE x.user_id = t.user_id AND x.transaction_id = t.transaction_id
                    AND (x.is_deleted IS NULL OR x.is_deleted = FALSE))
```

**Q-SPL-10 — compare-and-swap label write (G6 with `IS NULL` guard).** `ct/auto_suggest.rs:288`.
```sql
UPDATE split_transactions SET updated = NOW, label_category_id = $1, label_budget_id = $2, label_category_confidence = $3
WHERE split_transaction_id = $4 AND user_id = $5 AND label_category_confidence IS NULL AND LIVE RETURNING split_transaction_id
```

## 2.13 `budgets`, `sections`, `categories`

The three tables share one repository (`repo/budgets.rs`) and the same shapes.

**Q-BSC-1 — rows of the group, optionally narrowed to one id or one parent.** `repo/budgets.rs:29` via `:55` (budgets by `budget_id`), `:138` (sections by `budget_id`), `:186` (categories by `section_id`), `:81` (sections of a budget, before deleting it).
```sql
SELECT * FROM budgets    WHERE GROUP($1) [AND budget_id = $2]  AND LIVE
SELECT * FROM sections   WHERE GROUP($1) [AND budget_id = $2]  AND LIVE
SELECT * FROM categories WHERE GROUP($1) [AND section_id = $2] AND LIVE
```

**Q-BSC-2 — parent must exist, be live and be in the group (G3).** `repo/budgets.rs:146`, `:193`. The new child takes the parent row's `user_id`.
```sql
SELECT * FROM budgets  WHERE budget_id = $1  AND user_id = ANY(sharing_user_ids($2)) AND LIVE LIMIT $3
SELECT * FROM sections WHERE section_id = $1 AND user_id = ANY(sharing_user_ids($2)) AND LIVE LIMIT $3
```

**Q-BSC-3 — which of some ids the group owns, deleted ones included (`IN`, no soft-delete filter).** `repo/budgets.rs:118`, two tables queried concurrently (`:133`); before every transaction edit that names a label.
```sql
SELECT * FROM budgets    WHERE GROUP($1) AND budget_id   IN ($2, …)
SELECT * FROM categories WHERE GROUP($1) AND category_id IN ($2, …)
```

**Q-BSC-4 — live rows by ids within the group (G4).** `routes/accounts/post_suggest_category.rs:230`, `:231`.
```sql
SELECT * FROM categories WHERE category_id IN ($1, …) AND LIVE AND user_id = ANY(sharing_user_ids($k))
```

**Q-BSC-5 — insert, id from the server, whole row back (G5).** `repo/budgets.rs:69`, `:160`, `:207`. `capacities` is bound as a JSON array into JSONB. Inserting a section or category checks its parent FK.
```sql
INSERT INTO budgets (updated, user_id, name, iso_currency_code, roll_over[, roll_over_start_date], capacities) VALUES (NOW, $1, …) RETURNING *
INSERT INTO sections (updated, user_id, budget_id, name, roll_over[, roll_over_start_date], capacities) VALUES (NOW, $1, …) RETURNING *
INSERT INTO categories (updated, user_id, section_id, name, roll_over[, roll_over_start_date], capacities) VALUES (NOW, $1, …) RETURNING *
```

**Q-BSC-6 — partial update within the group (G6).** `repo/budgets.rs:50`.
```sql
UPDATE budgets SET updated = NOW, [name = $…] [, iso_currency_code = $…] [, roll_over = $…] [, roll_over_start_date = $…] [, capacities = $…] WHERE budget_id = $n AND GROUP($n+1) AND LIVE RETURNING budget_id
```

**Q-BSC-7 — soft-delete by ids within the group (G10).** `repo/budgets.rs:96`, `:178` (sections), `:99` (budget; its count is the route's answer).
```sql
UPDATE sections SET is_deleted = TRUE, updated = NOW WHERE section_id IN ($1, …) AND GROUP($k) RETURNING section_id
```

**Q-BSC-8 — soft-delete children by parent (G11).** `repo/budgets.rs:92`, `:177`.
```sql
UPDATE categories SET is_deleted = TRUE, updated = NOW WHERE section_id = $1 AND GROUP($2) RETURNING category_id
```

**Q-BSC-9 — soft-delete one row within the group (G9).** `repo/budgets.rs:219`.
```sql
UPDATE categories SET is_deleted = TRUE, updated = NOW WHERE category_id = $1 AND user_id = ANY(sharing_user_ids($2)) RETURNING category_id
```

`categories` and `sections` are also joined in Q-TXN-11.

## 2.14 `snapshots`

`GET /snapshots` chains up to three keyset streams (`repo/snapshots.rs:260`), each a G2 read in `snapshot_id` order with 500-row pages; the dates always bound `updated`, never `snapshot_date`.

**Q-SNP-1 — the group's rows.** `repo/snapshots.rs:103`.
```sql
SELECT * FROM snapshots
WHERE [GROUP($1)] [AND snapshot_type = $…] [AND account_id = $…] [AND security_id = $…] [AND account_id IN (…)]
  [AND updated >= $…] [AND updated <= $…] [AND LIVE] [AND snapshot_id > $…]
ORDER BY snapshot_id LIMIT $n
```

**Q-SNP-2 — the group's holding rows for some accounts** (holding rows keep the account in `holding_account_id`). `repo/snapshots.rs:126`.
```sql
SELECT * FROM snapshots
WHERE GROUP($1) AND snapshot_type = $2 [AND holding_account_id = $…] [AND holding_security_id = $…] [AND holding_account_id IN (…)]
  [AND updated >= $…] [AND updated <= $…] [AND LIVE] [AND snapshot_id > $…]
ORDER BY snapshot_id LIMIT $n
```

**Q-SNP-3 — global security-price rows (no owner filter; these have `user_id` NULL).** `repo/snapshots.rs:155`. Each batch is then enriched with Q-SEC-2 once per security not seen before.
```sql
SELECT * FROM snapshots WHERE snapshot_type = $1 [AND security_id = $2] [AND updated >= $…] [AND updated <= $…] [AND LIVE] [AND snapshot_id > $…] ORDER BY snapshot_id LIMIT $n
```
The comment at `:256` records why these are separate reads: the builder cannot say `user_id = $1 OR user_id IS NULL`.

**Q-SNP-4 — a security's price history in time order (range on a timestamp).** `repo/snapshots.rs:299`.
```sql
SELECT * FROM snapshots WHERE snapshot_type = $1 [AND security_id = $2] [AND snapshot_date >= $3] [AND snapshot_date <= $4] AND LIVE ORDER BY snapshot_date
```

**Q-SNP-5 — the group's holding history in time order.** `repo/snapshots.rs:358`.
```sql
SELECT * FROM snapshots WHERE GROUP($1) AND snapshot_type = $2 [AND holding_account_id = $3] [AND holding_security_id = $4] [AND snapshot_date >= $5] [AND snapshot_date <= $6] AND LIVE ORDER BY snapshot_date
```

**Q-SNP-6 — one holding snapshot by id within the group (G3).** `repo/snapshots.rs:380`. The id is bound as whatever JSON value the request carried.
```sql
SELECT * FROM snapshots WHERE snapshot_id = $1 AND snapshot_type = $2 AND user_id = ANY(sharing_user_ids($3)) AND LIVE LIMIT $4
```

**Q-SNP-7 — holding snapshots of one account for some securities, two-key order.** `repo/snapshots.rs:478`; read inside the plan-statement transaction.
```sql
SELECT * FROM snapshots WHERE GROUP($1) AND snapshot_type = $2 AND holding_account_id = $3 AND holding_security_id IN ($4, …) AND LIVE ORDER BY snapshot_date, snapshot_id
```

**Q-SNP-8 — date of a security's latest live price (`MAX`, zone conversion, formatting).** `ct/refresh_security_snapshots.rs:155`.
```sql
SELECT to_char(MAX(snapshot_date) AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS date
FROM snapshots WHERE security_id = $1 AND snapshot_type = 'security' AND LIVE
```

**Q-SNP-9 — which of some securities were written recently (`DISTINCT`, array parameter, server-clock window; tombstones count).** `ct/refresh_security_snapshots.rs:250`, the hourly job's cadence gate.
```sql
SELECT DISTINCT security_id FROM snapshots
WHERE security_id = ANY($1::text[]) AND snapshot_type = 'security' AND updated > NOW() - INTERVAL '22 hours'
```

**Q-SNP-10 — upsert an account-balance snapshot (G7 with allowlist; revives a tombstone).** `repo/snapshots.rs:605` (row built at `:544`).
```sql
INSERT INTO snapshots (updated, snapshot_id, user_id, snapshot_date, snapshot_type, account_id, is_deleted, balances_available, balances_current, balances_limit, balances_iso_currency_code)
VALUES (NOW, $1, …)
ON CONFLICT (snapshot_id) DO UPDATE SET snapshot_date = EXCLUDED.snapshot_date, is_deleted = EXCLUDED.is_deleted,
  balances_available = EXCLUDED.balances_available, …, updated = NOW RETURNING *
```

**Q-SNP-11 — upsert a security-price snapshot (G7, every supplied column; no owner, `is_deleted` not touched).** `repo/snapshots.rs:605` (row built at `:562`).
```sql
INSERT INTO snapshots (updated, snapshot_id, snapshot_date, snapshot_type, security_id, close_price) VALUES (NOW, $1, $2, $3, $4, $5)
ON CONFLICT (snapshot_id) DO UPDATE SET snapshot_date = EXCLUDED.snapshot_date, snapshot_type = EXCLUDED.snapshot_type,
  security_id = EXCLUDED.security_id, close_price = EXCLUDED.close_price, updated = NOW RETURNING *
```

**Q-SNP-12 — upsert a holding snapshot (G7 with allowlist; revives a tombstone).** `repo/snapshots.rs:446`, `:605` (row built at `:570`).
```sql
INSERT INTO snapshots (updated, snapshot_id, user_id, snapshot_date, snapshot_type, is_deleted, holding_account_id, holding_security_id[, institution_price][, institution_value][, cost_basis][, quantity])
VALUES (NOW, $1, …)
ON CONFLICT (snapshot_id) DO UPDATE SET snapshot_date = EXCLUDED.snapshot_date, is_deleted = EXCLUDED.is_deleted,
  holding_security_id = EXCLUDED.holding_security_id, <supplied amounts>, updated = NOW RETURNING *
```

**Q-SNP-13 — patch a holding snapshot within the group (G6).** `routes/accounts/post_holding_snapshot.rs:264`.
```sql
UPDATE snapshots SET updated = NOW, <fields the request named> WHERE snapshot_id = $n AND GROUP($n+1) AND LIVE RETURNING snapshot_id
```

**Q-SNP-14 — soft-delete one within the group (G9).** `repo/snapshots.rs:621`.
```sql
UPDATE snapshots SET is_deleted = TRUE, updated = NOW WHERE snapshot_id = $1 AND user_id = ANY(sharing_user_ids($2)) RETURNING snapshot_id
```

**Q-SNP-15 — soft-delete by ids within the group (G10).** `repo/snapshots.rs:503`.
```sql
UPDATE snapshots SET is_deleted = TRUE, updated = NOW WHERE snapshot_id IN ($1, …) AND GROUP($k) RETURNING snapshot_id
```

**Q-SNP-16 — soft-delete by account through either account column (G11).** `repo/accounts.rs:223`, `:224`, `repo/items.rs:191`, `:192`.
```sql
UPDATE snapshots SET is_deleted = TRUE, updated = NOW WHERE account_id = $1 | = ANY($1) AND GROUP($2) RETURNING snapshot_id
UPDATE snapshots SET is_deleted = TRUE, updated = NOW WHERE holding_account_id = $1 | = ANY($1) AND GROUP($2) RETURNING snapshot_id
```

## 2.15 `charts`

**Q-CHT-1 — charts of the group by optional id / type.** `repo/charts.rs:42`.
```sql
SELECT * FROM charts WHERE GROUP($1) [AND chart_id = $2] [AND type = $3] AND LIVE
```

**Q-CHT-2 — insert, id from the server (G5).** `repo/charts.rs:60`.
```sql
INSERT INTO charts (updated, user_id, name, type, configuration) VALUES (NOW, $1, $2, $3, $4) RETURNING *
```

**Q-CHT-3 — partial update within the group (G6).** `repo/charts.rs:81`.
```sql
UPDATE charts SET updated = NOW, [name = $…] [, type = $…] [, configuration = $…] WHERE chart_id = $n AND GROUP($n+1) AND LIVE RETURNING chart_id
```

**Q-CHT-4 — soft-delete one within the group (G9).** `repo/charts.rs:86`.
```sql
UPDATE charts SET is_deleted = TRUE, updated = NOW WHERE chart_id = $1 AND user_id = ANY(sharing_user_ids($2)) RETURNING chart_id
```

## 2.16 `api_keys`

Keys are never shared: every statement uses the caller's own `user_id`.

**Q-KEY-1 — insert, id from the server (G5).** `repo/api_keys.rs:55`. `scopes` is a JSON array bound into `TEXT[]`.
```sql
INSERT INTO api_keys (updated, user_id, name, key_hash, key_prefix, scopes, expires_at) VALUES (NOW, $1, $2, $3, $4, $5, $6) RETURNING key_id
```

**Q-KEY-2 — a user's unrevoked keys, newest first (projection without the hash).** `repo/api_keys.rs:64`.
```sql
SELECT key_id, user_id, name, key_prefix, scopes, created_at, last_used_at, revoked_at, expires_at
FROM api_keys WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC
```

**Q-KEY-3 — revoke once (G6 with `IS NULL` guard).** `repo/api_keys.rs:88`. A second revoke returns nothing → 404.
```sql
UPDATE api_keys SET updated = NOW, revoked_at = $1 WHERE key_id = $2 AND user_id = $3 AND revoked_at IS NULL RETURNING key_id
```

**Q-KEY-4 — look a key up by its hash (G3 on a unique column).** `repo/api_keys.rs:115`; every bearer-authenticated request.
```sql
SELECT * FROM api_keys WHERE key_hash = $1 LIMIT $2
```

**Q-KEY-5 — best-effort "last used" stamp (G6, fire and forget).** `repo/api_keys.rs:138`.
```sql
UPDATE api_keys SET updated = NOW, last_used_at = $1 WHERE key_id = $2 RETURNING key_id
```

## 2.17 `rejected_categories`

**Q-REJ-1 — record a rejection under the transaction's owner (`INSERT … SELECT` from another table with a sharing filter, composite conflict target, conditional `DO UPDATE … WHERE`).** `repo/rejected_categories.rs:34`. No row comes back when the transaction is outside the group (nothing selected) or the conflict's `WHERE` fails.
```sql
INSERT INTO rejected_categories (transaction_id, user_id, category_id)
SELECT t.transaction_id, t.user_id, $3 FROM transactions t
WHERE t.transaction_id = $1 AND t.user_id = ANY(sharing_user_ids($2))
ON CONFLICT (transaction_id, category_id) DO UPDATE SET rejected_at = CURRENT_TIMESTAMP
  WHERE rejected_categories.user_id = EXCLUDED.user_id
RETURNING *
```

**Q-REJ-2 — clear a rejection (hard delete on a three-column match).** `repo/rejected_categories.rs:66`. Row count returned.
```sql
DELETE FROM rejected_categories WHERE user_id = ANY(sharing_user_ids($1)) AND transaction_id = $2 AND category_id = $3
```

**Q-REJ-3 — copy rejections from one transaction to another (`INSERT … SELECT` from the same table, `ON CONFLICT DO NOTHING` on the composite key).** `repo/rejected_categories.rs:116`. The affected-row count (rows actually copied) is the function's result.
```sql
INSERT INTO rejected_categories (transaction_id, user_id, category_id, rejected_at)
SELECT $1, user_id, category_id, rejected_at FROM rejected_categories WHERE transaction_id = $2
ON CONFLICT (transaction_id, category_id) DO NOTHING
```

**Q-REJ-4 — delete all rejections of a transaction.** `repo/rejected_categories.rs:129`.
```sql
DELETE FROM rejected_categories WHERE transaction_id = $1
```

**Q-REJ-5 — rejections for many transactions, two-key order with mixed direction.** `repo/rejected_categories.rs:157`. *Unused.*
```sql
SELECT * FROM rejected_categories WHERE user_id = $1 AND transaction_id IN ($2, …) ORDER BY transaction_id, rejected_at DESC
```

`rejected_categories` is also joined in Q-TXN-11.

## 2.18 `cpi_snapshots`

**Q-CPI-1 — a whole series, oldest first.** `repo/cpi_snapshots.rs:13`.
```sql
SELECT * FROM cpi_snapshots WHERE series_id = $1 ORDER BY snapshot_date ASC
```

**Q-CPI-2 — bulk upsert (G8).** `repo/cpi_snapshots.rs:28`. `updated` is re-stamped even when the value is unchanged; the job depends on that.
```sql
INSERT INTO cpi_snapshots (updated, cpi_snapshot_id, series_id, snapshot_date, index_value) VALUES (NOW, $1, $2, $3, $4), (NOW, …), …
ON CONFLICT (cpi_snapshot_id) DO UPDATE SET series_id = EXCLUDED.series_id, snapshot_date = EXCLUDED.snapshot_date, index_value = EXCLUDED.index_value, updated = NOW RETURNING *
```

## 2.19 Sharing: `sharing_members`, `sharing_invitations`, `sharing_email_invitations`

**Q-SHR-1 — the caller's sharing group as rows.** `repo/sharing.rs:137`. After **every** successful mutation (event fan-out, `srv/realtime.rs:160`), in sharing changes, and at the start of each transfer-detection run.
```sql
SELECT unnest(sharing_user_ids($1)) AS user_id
```

**Q-SHR-2 — members of the caller's group with names (join, scalar subquery, ordered by name).** `repo/sharing.rs:141`. Empty when the caller shares with nobody.
```sql
SELECT u.user_id, u.username FROM sharing_members m
JOIN users u ON u.user_id = m.user_id
WHERE m.group_id = (SELECT own.group_id FROM sharing_members own WHERE own.user_id = $1)
ORDER BY u.username
```

**Q-SHR-3 — invitations that concern the caller (same table joined twice to `users`, `OR` of a direct match and a group match).** `repo/sharing.rs:150` (select list at `:86`).
```sql
SELECT i.invitation_id,
       inviter.user_id AS inviter_user_id, inviter.username AS inviter_username,
       invitee.user_id AS invitee_user_id, invitee.username AS invitee_username
FROM sharing_invitations i
JOIN users inviter ON inviter.user_id = i.inviter_user_id
JOIN users invitee ON invitee.user_id = i.invitee_user_id
WHERE (i.invitee_user_id = $1 OR i.inviter_user_id = ANY(sharing_user_ids($1)))
ORDER BY invitee.username, i.invitation_id
```

**Q-SHR-4 — email invitations that concern the caller (boolean expression as an output column, scalar subquery, server-clock expiry).** `repo/sharing.rs:160`. Q-SHR-2, -3 and -4 run concurrently on three connections for `GET /sharing` (`:179`).
```sql
SELECT e.invitation_id, e.invitee_email, e.expires_at,
       inviter.user_id AS inviter_user_id, inviter.username AS inviter_username,
       e.inviter_user_id = ANY(sharing_user_ids($1)) AS sent
FROM sharing_email_invitations e
JOIN users inviter ON inviter.user_id = e.inviter_user_id
WHERE (e.inviter_user_id = ANY(sharing_user_ids($1))
       OR (e.invitee_email = (SELECT lower(u.email) FROM users u WHERE u.user_id = $1) AND e.expires_at > CURRENT_TIMESTAMP))
ORDER BY e.invitee_email, e.invitation_id
```

**Q-SHR-5 — owner of a row the caller may act on (generic over table and key; join to `users`).** `repo/sharing.rs:202`. Called with `accounts`, `items`, `transactions` and `recurrings` before creating a child row, so the child takes its parent's owner.
```sql
SELECT u.user_id, u.username FROM <table> r JOIN users u ON u.user_id = r.user_id
WHERE r.<pk> = $1 AND r.user_id = ANY(sharing_user_ids($2)) [AND (r.is_deleted IS NULL OR r.is_deleted = FALSE)]
```

**Q-SHR-6 — an invitation by its inviter/invitee pair.** `repo/sharing.rs:235` (G3, the crossing invitation), `:246` (G1, re-read after insert).
```sql
SELECT * FROM sharing_invitations WHERE inviter_user_id = $1 AND invitee_user_id = $2 [LIMIT $3]
```

**Q-SHR-7 — invite once (G5 with `ON CONFLICT DO NOTHING`).** `repo/sharing.rs:245`. A repeat is a silent no-op; the row is then re-read with Q-SHR-6.
```sql
INSERT INTO sharing_invitations (updated, inviter_user_id, invitee_user_id) VALUES (NOW, $1, $2) ON CONFLICT DO NOTHING RETURNING invitation_id
```

**Q-SHR-8 — a user's email invitations (to count them under a lock).** `repo/sharing.rs:326`.
```sql
SELECT * FROM sharing_email_invitations WHERE inviter_user_id = $1
```

**Q-SHR-9 — send or renew an email invitation (G7 on a non-primary unique key).** `repo/sharing.rs:341`.
```sql
INSERT INTO sharing_email_invitations (updated, inviter_user_id, invitee_email, expires_at) VALUES (NOW, $1, $2, $3)
ON CONFLICT (inviter_user_id, invitee_email) DO UPDATE SET expires_at = EXCLUDED.expires_at, updated = NOW RETURNING *
```

**Q-SHR-10 — decline or withdraw an invitation (conditional `DELETE … RETURNING`).** `repo/sharing.rs:357`. The returned row is both the authorization check and the list of users to notify.
```sql
DELETE FROM sharing_invitations i
WHERE i.invitation_id = $2 AND (i.invitee_user_id = $1 OR i.inviter_user_id = ANY(sharing_user_ids($1)))
RETURNING i.inviter_user_id, i.invitee_user_id
```

**Q-SHR-11 — the same for an email invitation (`RETURNING` with a correlated scalar subquery).** `repo/sharing.rs:365`.
```sql
DELETE FROM sharing_email_invitations e
WHERE e.invitation_id = $2
  AND (e.inviter_user_id = ANY(sharing_user_ids($1))
       OR (e.invitee_email = (SELECT lower(u.email) FROM users u WHERE u.user_id = $1) AND e.expires_at > CURRENT_TIMESTAMP))
RETURNING e.inviter_user_id, (SELECT u.user_id FROM users u WHERE lower(u.email) = e.invitee_email) AS invitee_user_id
```

**Q-SHR-12 — find an open invitation of either kind (`UNION ALL`).** `repo/sharing.rs:398`.
```sql
SELECT inviter_user_id FROM sharing_invitations WHERE invitation_id = $1 AND invitee_user_id = $2
UNION ALL
SELECT e.inviter_user_id FROM sharing_email_invitations e
WHERE e.invitation_id = $1 AND e.inviter_user_id <> $2
  AND (e.invitee_email = (SELECT lower(u.email) FROM users u WHERE u.user_id = $2) AND e.expires_at > CURRENT_TIMESTAMP)
```

**Q-SHR-13 — memberships of two users (`IN`).** `repo/sharing.rs:447`.
```sql
SELECT * FROM sharing_members WHERE user_id IN ($1, $2)
```

**Q-SHR-14 — add both users to a group unless already members (G8 with an empty update list).** `repo/sharing.rs:472`.
```sql
INSERT INTO sharing_members (updated, user_id, group_id) VALUES (NOW, $1, $2), (NOW, $3, $4) ON CONFLICT (user_id) DO NOTHING RETURNING *
```

**Q-SHR-15 — delete invitations between two members of a group (`DELETE … USING`, same table twice).** `repo/sharing.rs:410`.
```sql
DELETE FROM sharing_invitations i
USING sharing_members inviter, sharing_members invitee
WHERE inviter.user_id = i.inviter_user_id AND invitee.user_id = i.invitee_user_id
  AND inviter.group_id = $1 AND invitee.group_id = $1
```

**Q-SHR-16 — the same for email invitations (three-table `USING`, expression join).** `repo/sharing.rs:419`.
```sql
DELETE FROM sharing_email_invitations e
USING sharing_members inviter, sharing_members invitee, users u
WHERE inviter.user_id = e.inviter_user_id AND u.user_id = invitee.user_id AND lower(u.email) = e.invitee_email
  AND inviter.group_id = $1 AND invitee.group_id = $1
```

**Q-SHR-17 — remove members (hard delete by an array of keys).** `repo/sharing.rs:504`.
```sql
DELETE FROM sharing_members WHERE user_id = ANY($1::uuid[])
```

## 2.20 Locks, transaction control, health and DDL

**Q-SYS-1 — health probe.** `routes/health.rs:10`. `SELECT 1`

**Q-SYS-2 — transaction control.** `BEGIN`, `COMMIT`, `ROLLBACK` as separate statements on a reserved connection (`pg/client.rs:446`, and written out in `repo/transfers.rs`, `repo/recurrings.rs`, `repo/rejected_categories.rs`, `ct/detect_*.rs`, `pg/migration.rs`). Never `BEGIN ISOLATION LEVEL …`, never `SET`.

**Q-SYS-3 — savepoints.** `ct/detect_transfers.rs:284`, `:287`, `:293`. `SAVEPOINT pair_attempt`, `RELEASE SAVEPOINT pair_attempt`, `ROLLBACK TO SAVEPOINT pair_attempt`

**Q-SYS-4 — session advisory lock and unlock.** `pg/migration.rs:169`, `:211`. `SELECT pg_advisory_lock($1)`, `SELECT pg_advisory_unlock($1)` with the constant 6453604.

**Q-SYS-5 — transaction advisory lock, exclusive, constant key.** `repo/sharing.rs:131`. `SELECT pg_advisory_xact_lock(hashtext('sharing'))`

**Q-SYS-6 — transaction advisory lock, shared, same key.** `repo/transfers.rs:37`. `SELECT pg_advisory_xact_lock_shared(hashtext('sharing'))`

**Q-SYS-7 — transaction advisory lock keyed on the sharing group.** `repo/transfers.rs:38`.
```sql
SELECT pg_advisory_xact_lock(hashtext((SELECT min(member::text) FROM unnest(sharing_user_ids($1)) member) || ':transfers'))
```

**Q-SYS-8 — transaction advisory lock keyed on a user and a purpose.** `repo/recurrings.rs:269` and `ct/detect_recurrings.rs:697` (`':recurrings'`), `repo/sharing.rs:279` (`':email-invitations'`).
```sql
SELECT pg_advisory_xact_lock(hashtext($1 || ':recurrings'))
SELECT pg_advisory_xact_lock(hashtext($1 || ':email-invitations'))
```

**Q-DDL-1 — extension.** `pg/initialize.rs:38`. `CREATE EXTENSION IF NOT EXISTS pg_trgm`

**Q-DDL-2 — tables.** `pg/initialize.rs:41`, built by `pg/database.rs:107`. `CREATE TABLE IF NOT EXISTS <t> (<column definition>, …, <table constraint>, …)` with the definitions exactly as the model text gives them (inline `PRIMARY KEY`, `UNIQUE`, `REFERENCES … ON DELETE …`, `DEFAULT …`, `NOT NULL`).

**Q-DDL-3 — function.** `pg/initialize.rs:45`. `CREATE OR REPLACE FUNCTION sharing_user_ids(viewer UUID) RETURNS UUID[] LANGUAGE sql STABLE PARALLEL SAFE AS $$ … $$`

**Q-DDL-4 — catalog read.** `pg/migration.rs:116`.
```sql
SELECT column_name, data_type, udt_name, is_nullable, column_default
FROM information_schema.columns WHERE table_name = $1 AND table_schema = 'public' ORDER BY ordinal_position
```

**Q-DDL-5 — add a column.** `pg/migration.rs:136`, text built at `:88`. `ALTER TABLE <t> ADD COLUMN IF NOT EXISTS <column> <definition without PRIMARY KEY, with an inferred DEFAULT when NOT NULL has none>`

**Q-DDL-6 — duplicate-key check before a unique index (`GROUP BY … HAVING COUNT(*) > 1`).** `pg/initialize.rs:21`, built at `pg/database.rs:121`. Today it only ever runs for `lower(email)` on `users`.
```sql
SELECT lower(email) FROM users WHERE lower(email) IS NOT NULL AND (email IS NOT NULL)
GROUP BY lower(email) HAVING COUNT(*) > 1 ORDER BY lower(email)
```

**Q-DDL-7 — indexes.** `pg/initialize.rs:61`, built at `pg/database.rs:113`. `CREATE [UNIQUE] INDEX IF NOT EXISTS <name> ON <t>(<columns or expression>) [WHERE <predicate>]`; the name defaults to `idx_<table>_<col>_<col>`.

## 2.21 Coverage against the checklist

| asked about | where it occurs |
|---|---|
| point lookups | Q-USR-1/2, Q-SES-1, Q-SEC-2, Q-ITM-1, Q-ACC-2, Q-TXN-1, Q-INV-1, Q-SNP-6, Q-KEY-4, Q-BSC-2, Q-SHR-6 |
| equality filters | every builder read (2.0.2) |
| ranges | `updated >= / <=` (Q-TXN-2, Q-INV-2, Q-SPL-1, Q-SNP-1..3, Q-PAR-1, Q-REC-1), `date >= / <=` (Q-TXN-5, Q-INV-3/4), `snapshot_date >= / <=` (Q-SNP-4/5), `cookie_expires <= $1` (Q-SES-7), `BETWEEN` (Q-TXN-11, Q-TXN-14), `expires_at > CURRENT_TIMESTAMP` (Q-SHR-4/11/12), `> NOW() - INTERVAL` (Q-SPL-8, Q-SNP-9), `ABS(amount) > $` and `date >= $::date` (Q-SPL-9), confidence bands (Q-TXN-19) |
| `IN` list / `= ANY(array)` | builder `IN ($1,…)` (G4, `in_filters`); `= ANY($n)` with untyped, `::text[]` or `::uuid[]` arrays (G11, G12, Q-TXN-9/10/13/14/19, Q-PAR-9/10/11, Q-SNP-9, Q-SPL-9, Q-SHR-17); `= ANY(sharing_user_ids($n))` everywhere; `= ANY(array column)` (Q-REC-12); `x IN (a, b)` over two columns or two parameters (Q-TXN-14, Q-PAR-9, Q-PAR-10) |
| `LIKE` / `ILIKE` | `LIKE 'TRANSFER%'` on a JSON text value (Q-TXN-14). No `ILIKE` anywhere. |
| regex | `~` and `SUBSTRING … FROM` (Q-TXN-7, Q-INV-6) |
| fuzzy text | `similarity()` from pg_trgm (Q-TXN-11) |
| case-insensitive match | `lower(col) = lower($n)` (Q-USR-3), `lower(u.email) = e.invitee_email` (Q-SHR-4/11/12/16) |
| `IS NULL` / `IS NOT NULL` | builder null filter (Q-TXN-6), update guards (Q-USR-6, Q-KEY-3, Q-TXN-18, Q-SPL-10), `revoked_at IS NULL` (Q-KEY-2), `security_id IS NOT NULL` (Q-HLD-6), the `LIVE` predicate, `IS [NOT] DISTINCT FROM` (Q-TXN-19) |
| JSON operators | `->` then `->>` on `transactions.raw` (Q-TXN-11, Q-TXN-14). Nothing else: no containment, no `jsonb_set`, no JSON path, no JSON index. |
| array operators | `&&` (Q-REC-8), `\|\|` (Q-REC-9/10/11), `UNNEST` (Q-REC-9/10, Q-SPL-9, Q-SHR-1, Q-SYS-7), `ARRAY(SELECT …)`, `array_append` (the function) |
| `ORDER BY` + `LIMIT` | Q-TXN-10, Q-TXN-11, Q-TXN-14, Q-SEC-3 (order only), `LIMIT 1` probes (Q-PAR-2, Q-REC-8, Q-SPL-6, Q-TXN-8) |
| `OFFSET` | Q-INV-4 only, unused |
| keyset pagination | G2: Q-TXN-2, Q-INV-2, Q-SNP-1/2/3 |
| inner joins | Q-SEC-1, Q-ITM-4, Q-TXN-11, Q-TXN-12, Q-TXN-14 (self-join plus two), Q-SPL-8, Q-SHR-2/3/4/5; `UPDATE … FROM` (Q-PAR-11); `DELETE … USING` (Q-SHR-15/16); `CROSS JOIN UNNEST` (Q-SPL-9) |
| left / outer joins | none |
| subqueries | scalar (Q-SHR-2/4/11/12, Q-TXN-11, Q-SYS-7), `IN (SELECT …)` (G12 / Q-PAR-8), in `SET` (Q-REC-9/10), in `RETURNING` (Q-SHR-11) |
| `EXISTS` / `NOT EXISTS` | Q-TXN-12, Q-TXN-14, Q-PAR-9, Q-PAR-10, Q-REC-12, Q-SPL-9 |
| CTEs | Q-TXN-11 only (non-recursive, read-only) |
| `UNION` | `UNION ALL` in Q-SHR-12 only |
| aggregates | `COUNT(*)` (Q-TXN-10, Q-TXN-11, Q-DDL-6), `SUM` (Q-TXN-11), `MAX` (Q-TXN-7, Q-TXN-10, Q-INV-6, Q-SNP-8), `MIN` (Q-SYS-7), `GROUP BY` (Q-TXN-10, Q-TXN-11, Q-DDL-6), `HAVING` (Q-DDL-6) |
| `DISTINCT` | Q-SEC-1 (whole rows), Q-HLD-6, Q-SNP-9, inside arrays (Q-REC-9/10). No `DISTINCT ON`. |
| window functions | none |
| single-row `INSERT … RETURNING` | G5: Q-USR-7, Q-ACC-5, Q-TXN-16, Q-INV-9, Q-SPL-3, Q-BSC-5, Q-CHT-2, Q-KEY-1, Q-SHR-7 |
| bulk insert | multi-row `VALUES` (G8: Q-INS-1, Q-CPI-2, Q-INV-8, Q-SHR-14); `INSERT … SELECT` (Q-SPL-9 with `UNNEST`, Q-REJ-1, Q-REJ-3, Q-PAR-10) |
| `ON CONFLICT` | target-less `DO NOTHING` (Q-USR-7, Q-SHR-7); on the primary key `DO UPDATE` (G7/G8) or `DO NOTHING` (Q-INV-8, Q-SHR-14); on a multi-column key that is not a single-column primary key — a unique constraint (Q-PAR-3, Q-REC-9, Q-REC-13, Q-SHR-9) or the composite primary key (Q-REJ-1, Q-REJ-3); conditional `DO UPDATE … WHERE` (Q-REC-13, Q-REJ-1); `CASE` over the existing row (Q-PAR-3, Q-REC-9) |
| `UPDATE` with expressions | `x = x \|\| $` (Q-REC-11), array union subquery (Q-REC-9/10), `CASE` (Q-PAR-3, Q-REC-9), casts of parameters (Q-TXN-19, Q-REC-5). No `x = x + 1`, no `COALESCE` in a `SET`, no jsonb function. |
| conditional `UPDATE`/`DELETE` whose count or `RETURNING` is relied on | every G6/G9/G10/G11 call (2.0.4), plus Q-USR-6, Q-KEY-3, Q-REC-6, Q-SPL-7, Q-REJ-2, Q-REJ-3, Q-PAR-9, Q-REC-12, Q-SHR-10, Q-SHR-11, Q-REC-13 |
| `DELETE` with subquery / join | Q-SHR-11 (subquery in predicate and in `RETURNING`), Q-SHR-15/16 (`USING`) |
| soft deletes | G9–G12 and the raw ones (Q-PAR-9, Q-PAR-11, Q-REC-12) |
| hard deletes | sessions (Q-SES-5/6/7), rejected_categories (Q-REJ-2/4), sharing (Q-SHR-10/11/15/16/17). Nothing else is ever deleted. |
| `TRUNCATE` | never |
| row locks | `FOR SHARE` (Q-TXN-9), `FOR UPDATE` (Q-PAR-5) |
| advisory locks | Q-SYS-4..8 |

---

# 3. Transactions and concurrency

## 3.1 The model the code assumes

- **Autocommit by default.** `pool().query(…)` checks a connection out for one statement (`pg/client.rs:390`). Most repository functions are one or a few independent autocommit statements; loops of upserts are *not* atomic unless listed below.
- **Explicit transactions run on a reserved connection** with literal `BEGIN` / `COMMIT` / `ROLLBACK` statements, either through `with_transaction` (`pg/client.rs:440`) or written out by hand. A repository function takes an optional executor so the same statement can run inside or outside a transaction.
- **Isolation level is never set.** Everything runs at the server default, READ COMMITTED, and the code comments reason in those terms ("READ COMMITTED hides the in-flight write", `repo/transfers.rs:32`; `ct/detect_transfers.rs:371`). No statement uses `SERIALIZABLE`, `REPEATABLE READ`, `SET TRANSACTION`, `SET LOCAL` or deferred constraints.
- **Reads issued on the pool from inside a function that holds a transaction see committed data only**, and this is sometimes deliberate (`repo/split_transactions.rs:134`: splits are "looked up on the pool, outside the caller's transaction, and deleted on `client`").
- **Statement-level effects relied on under READ COMMITTED:** a statement that blocks on a row lock re-checks its `WHERE` against the row's new version once the lock is released. `pair_transactions` depends on this: its `FOR SHARE` read waits for a concurrent soft delete and then no longer returns the row.
- **A failed statement poisons the transaction.** The code knows that after an error every later statement in that transaction fails and that `COMMIT` then rolls back *without raising* (`repo/holdings.rs:53`). Per-row upsert helpers swallow errors and return a 500 status for the row; called inside a transaction (`ct/sync_simple_fin.rs:156`) that means either a later statement whose error is *not* swallowed raises and the whole block rolls back, or — when every later statement is itself swallowed or skipped — the block reports success and stores nothing.

## 3.2 Every multi-statement transaction

| # | where | statements grouped, in order | notes |
|---|---|---|---|
| T1 | schema migration, `pg/migration.rs:166` | session advisory lock (before `BEGIN`); `BEGIN`; per table: catalog read (Q-DDL-4), `ALTER TABLE … ADD COLUMN` (Q-DDL-5); `COMMIT` or `ROLLBACK`; advisory unlock (after the transaction, always) | DDL inside a transaction; all column additions land atomically or not at all |
| T2 | `delete_accounts`, `repo/accounts.rs:210` | per account: soft-delete its transactions, investment transactions, splits, balance snapshots, holding snapshots, holdings (six G11 statements); then pairs through both sides (Q-PAR-8); then the accounts (Q-ACC-9) | the pair cascade must come after the transaction soft delete, whose row locks block a concurrent pairing |
| T3 | `delete_item`, `repo/items.rs:167` | the same cascade with `= ANY(array)` over all the item's accounts, then accounts, then the item (Q-ITM-8) | the account list is read *before* the transaction, on the pool |
| T4 | `delete_budget`, `repo/budgets.rs:88` | categories by section (Q-BSC-8, per section), sections (Q-BSC-7), budget (Q-BSC-7) | sections are read before the transaction |
| T5 | `delete_section`, `repo/budgets.rs:174` | categories by section, then the section | |
| T6 | `invite_by_email`, `repo/sharing.rs:311` | per-user advisory lock (Q-SYS-8); read the user's email invitations (Q-SHR-8); app-side count against the cap of 20; upsert (Q-SHR-9) | read-count-then-write made safe by the lock |
| T7 | `accept_sharing_invitation`, `repo/sharing.rs:436` | exclusive `sharing` lock (Q-SYS-5); find the invitation (Q-SHR-12); memberships (Q-SHR-13); add members (Q-SHR-14); delete settled invitations (Q-SHR-15, Q-SHR-16); read the new group (Q-SHR-1) | read-your-own-writes inside the transaction: the final read must see the rows just inserted |
| T8 | `remove_sharing_member`, `repo/sharing.rs:493` | exclusive `sharing` lock; read the group (Q-SHR-1); delete members (Q-SHR-17); soft-delete cross-owner pairs (Q-PAR-11) | |
| T9 | `migrate_rejected_categories_on_pending_posted`, `repo/rejected_categories.rs:109` | copy (Q-REJ-3), delete the originals (Q-REJ-4) | move-rows; a failed `ROLLBACK` is ignored |
| T10 | `pair_transactions`, `repo/transfers.rs:293` | shared `sharing` lock + group `transfers` lock (Q-SYS-6, Q-SYS-7); existence with `FOR SHARE` (Q-TXN-9); collision probe (Q-PAR-2); upsert (Q-PAR-3); reject other suggestions (Q-PAR-4) | early exits issue `ROLLBACK` and return a domain failure, not an error |
| T11 | `confirm_transfer_pair`, `repo/transfers.rs:465` | same two locks; read `FOR UPDATE` (Q-PAR-5); collision probe (Q-PAR-2); confirm (Q-PAR-6); reject other suggestions (Q-PAR-4) | |
| T12 | `confirm_recurring`, `repo/recurrings.rs:563` | `recurrings` lock (Q-SYS-8); confirm with `RETURNING` (Q-REC-6); then `label_confirmed_recurring`: Q-REC-3, optionally Q-TXN-10 and Q-REC-5, then Q-TXN-19 | |
| T13 | `set_recurring_label`, `repo/recurrings.rs:600` | lock; read (Q-REC-4); write label (Q-REC-5); when confirmed: Q-TXN-10 if needed, then Q-TXN-19 | |
| T14 | `create_recurring`, `repo/recurrings.rs:779` | lock; existence `FOR SHARE` (Q-TXN-9); overlap probe (Q-REC-8); upsert (Q-REC-9); `label_confirmed_recurring` | |
| T15 | `add_to_recurring`, `repo/recurrings.rs:912` | lock; read recurring (Q-REC-4); transactions `FOR SHARE` with amounts (Q-TXN-9); overlap probe (Q-REC-8); union members (Q-REC-10); `label_confirmed_recurring` with the new ids: Q-REC-3, Q-SPL-9, Q-TXN-19 | |
| T16 | recurring detection, one per user, `ct/detect_recurrings.rs:918` / `:696` | lock; stale cleanup (Q-REC-12); candidates (Q-TXN-12); all recurrings (Q-REC-2); per group Q-REC-13 or Q-REC-14; members (Q-TXN-13); per confirmed recurring Q-REC-11 plus `label_confirmed_recurring`; `COMMIT` | long transaction holding a per-user lock; a failure rolls back that user only |
| T17 | transfer detection, one per sharing group, `ct/detect_transfers.rs:374` / `:220` | both locks; read group (Q-SHR-1); stale cleanup (Q-PAR-9); candidates (Q-TXN-14); per candidate: `SAVEPOINT`, Q-PAR-10, `RELEASE` or `ROLLBACK TO SAVEPOINT`; `COMMIT` | the only use of savepoints: a failed insert must not lose the pairs created earlier in the loop |
| T18 | SimpleFin sync, phase 2, `ct/sync_simple_fin.rs:156` | upsert transactions (Q-TXN-15 each); delete removed transactions with their splits and pairs (Q-SPL-5, Q-PAR-7, Q-TXN-20); per removed id Q-SPL-1 (on the pool) and Q-SPL-4; upsert investment transactions (Q-INV-7 each); delete removed ones (Q-INV-11); upsert the item (Q-ITM-5) | atomic so the watermark only advances with the data; can be thousands of statements in one transaction |
| T19 | plan-statement import, `srv/plan_statement/import.rs:325` | bulk insert in chunks of 1000 (Q-INV-8); store mappings (Q-ACC-8); when replacing: read holding snapshots (Q-SNP-7) and soft-delete them (Q-SNP-15); upsert holdings (Q-HLD-3 each) | |

Not transactional, by design: the Plaid sync (`ct/sync_plaid.rs:486`, idempotent upserts then the cursor last), `upsert_*` loops, `delete_transactions` when called from a route (four autocommit statements), `reject_transfer_pair`, `set_recurring_splits`, `dismiss_recurring`, `rename_recurring`, `invite_to_share` (check, check, insert-do-nothing, re-read).

Rust/TypeScript difference in `with_transaction`: when `COMMIT` itself fails, TypeScript sends a `ROLLBACK` before rethrowing (`ts/lib/postgres/client.ts:252`), Rust returns the error without one (`pg/client.rs:449`). When the work fails and the `ROLLBACK` fails too, Rust reports the `ROLLBACK`'s error, TypeScript the same. The hand-written transactions differ among themselves on that point (`repo/transfers.rs:247` passes the `ROLLBACK` failure on, `repo/recurrings.rs:544` ignores it).

## 3.3 Row locks

| statement | lock | what it is for |
|---|---|---|
| Q-TXN-9 in `pair_transactions` (`repo/transfers.rs:315`), `create_recurring` (`repo/recurrings.rs:794`), `add_to_recurring` (`repo/recurrings.rs:944`) | `SELECT … FOR SHARE` on `transactions` rows | conflicts with the exclusive row lock a soft-delete `UPDATE` takes, so "these transactions exist and are live" stays true until commit; a delete that got there first makes the read return fewer rows |
| Q-PAR-5 in `confirm_transfer_pair` (`repo/transfers.rs:482`) | `SELECT … FOR UPDATE` on one `transaction_pairs` row | a concurrent cascade that soft-deletes the pair waits until the confirm commits |
| implicit | every `UPDATE` / `DELETE` holds its rows until commit | `repo/transactions.rs:329` orders the account cascade around it |

No `SKIP LOCKED`, no `NOWAIT`, no `FOR NO KEY UPDATE` / `FOR KEY SHARE`, no table locks. Lock waits are unbounded.

## 3.4 Advisory locks

| key | mode and scope | taken by | serializes |
|---|---|---|---|
| integer `6453604` | exclusive, **session** (explicit unlock) | T1 | schema migration across processes starting at once |
| `hashtext('sharing')` | exclusive, transaction | T7, T8 | all changes to sharing membership |
| `hashtext('sharing')` | **shared**, transaction | T10, T11, T17 | holds membership still while a group's pairs are written; always taken before the next lock, so the two never wait on each other in turn (`pg/database.rs:54`) |
| `hashtext(<smallest member id as text> \|\| ':transfers')`, computed in SQL from `sharing_user_ids($1)` | exclusive, transaction | T10, T11, T17 | every transfer-pair writer of one sharing group: manual pairing, confirming, the hourly job |
| `hashtext(<user id> \|\| ':recurrings')` | exclusive, transaction | T12–T16 | every recurring writer of one user |
| `hashtext(<user id> \|\| ':email-invitations')` | exclusive, transaction | T6 | one user's invitation sends, so two cannot both pass the cap |

Requirements this implies: named mutual-exclusion locks that live exactly as long as a transaction, a shared/exclusive pair on one key, a lock that outlives transactions on one connection, blocking acquisition, and keys derived from data read in the same statement. `hashtext` gives a 32-bit key, so unrelated names can collide; the code tolerates that (it only costs waiting).

## 3.5 Errors the app depends on

- **No SQLSTATE is ever inspected.** `DbError.code` is captured and `sqlstate()` exists (`pg/client.rs:64`) but has no caller in Rust, and the TypeScript never reads an error code either. Every database error is handled generically: logged, turned into a 500 for that row or that request, or rolls a transaction back.
- **Unique violations are avoided declaratively** with `ON CONFLICT`: target-less `DO NOTHING` standing for "any unique key is taken" (Q-USR-7, Q-SHR-7), `DO NOTHING` on a key (Q-INV-8, Q-SHR-14, Q-REJ-3), `DO UPDATE` everywhere else.
- **One place expects a unique violation as an error:** the pair insert in transfer detection (Q-PAR-10). When a pair for the same two transactions exists under an owner outside the bound group, the unique constraint raises (23505); the code catches the failure generically, rolls back to its savepoint, logs "Failed to insert transfer pair" and continues (`ct/detect_transfers.rs:285`). `conformance/README.md` pins this.
- **Type errors are expected to be errors, not empty results.** A non-UUID string compared with a UUID column (22P02), an unreadable timestamp (22007/22008), a number bound against a text column (42883): the conformance suite pins that `GET /recurrings` with a bad `start-date` and `POST`/`DELETE /recurrings` with a non-UUID id answer 500 and raise an alarm; `pg/client.rs:6` documents that `WHERE text_col = 5` fails on both servers alike; routes validate UUIDs up front precisely because one bad id would otherwise "fail the whole batch" (`routes/accounts/post_suggest_category.rs:150`).
- **Constraint errors surface as per-row failures:** foreign key (23503, e.g. rejecting a category that does not exist — swallowed by the caller, `srv/record_category_rejection.rs:70`), NOT NULL (23502), over-long `VARCHAR` (22001), numeric overflow (22003). Routes bound amounts to the column precision before writing (`DECIMAL_15_2_MAX = 9999999999999.99`, `DECIMAL_15_6_MAX = 999999999.999999`, `srv/validation.rs:181`).
- **Serialization failures and deadlocks are not handled.** None is expected at READ COMMITTED with the lock ordering above; if one occurred it would be a plain 500 or a failed job run.
- **Retries:** none for any statement. The only retry is the connection at start, every 10 s forever (`pg/initialize.rs:69`). Background jobs simply run again an hour later.
- **Timeouts:** connect timeout 2 s (both servers); idle timeout 30 s (TypeScript only). No `statement_timeout`, `lock_timeout` or `idle_in_transaction_session_timeout` is set by the app.

## 3.6 Concurrency outside the database

- Statements are issued concurrently on separate connections with `futures::join!` / `Promise.all` (`GET /sharing`: three; account-window read: two; label ownership: two; `POST /suggest-category`: three reads then updates in bounded chunks; Plaid sync: all re-points, deletes and migrations of a batch at once). The pool of 20 is the only bound.
- Compare-and-swap in a single statement replaces read-modify-write in several places: Q-USR-6, Q-KEY-3, Q-TXN-18, Q-SPL-10, Q-REC-13, Q-PAR-3.
- In-process guards that do **not** coordinate across processes: one scheduled sync at a time (`ct/schedule.rs:29`), one SSO re-check per session at a time (`srv/session.rs:139`), ordered event fan-out (`srv/realtime.rs:186`). Two processes can therefore run the hourly item sync at once; that is safe only because the writes are idempotent upserts and the detection jobs take advisory locks.

---

# 4. Other server features

## 4.1 What is *not* used

`LISTEN` / `NOTIFY` (none in either server: real-time updates are an in-process registry of server-sent-event streams with a 512-entry, 15-minute replay buffer per user, `srv/realtime.rs`), full-text search (`tsvector`, `@@`), sequences, `bytea`, large objects, triggers, rules, views, stored procedures other than the one SQL function, window functions, recursive CTEs, `DISTINCT ON`, outer joins, `TRUNCATE`, `COPY`, cursors, temp tables, `EXPLAIN`, partitioning, row-level security, roles/grants, generated columns, enum types, range types, `ILIKE`, JSON containment or path operators, GIN/GiST indexes, two-phase commit, replication features.

## 4.2 Server time

| use | where |
|---|---|
| `updated = CURRENT_TIMESTAMP` on every builder write and most raw writes | 2.0; in Postgres this is the **transaction start time**, so every row written in one transaction carries the same `updated`, and a long transaction commits rows whose `updated` lies in the past |
| column defaults `CURRENT_TIMESTAMP` | `updated`, `created_at`, `rejected_at` |
| `expires_at > CURRENT_TIMESTAMP` | Q-SHR-4, Q-SHR-11, Q-SHR-12 (the expiry itself is computed from the app's clock, `repo/sharing.rs:336`) |
| `NOW() - INTERVAL '1 week'`, `NOW() - INTERVAL '22 hours'` | Q-SPL-8, Q-SNP-9 |

`updated` is the app's change feed. Clients keep a cursor (their own wall clock at the start of the last successful sync, minus 60 s, `src/client/lib/hooks/sync.ts:176`) and ask for `updated >= cursor`; soft-deleted rows are included as tombstones. So: `updated` must be a wall-clock instant comparable with client-supplied ISO timestamps, must move forward on every change including soft deletes, and the 60 s margin is all that covers clock skew and commit delay. Other consumers of `updated`: `ORDER BY updated DESC` to pick the newest security (Q-SEC-3), the 22-hour cadence gates (Q-SNP-9; CPI refresh reads `updated` from Q-CPI-1), the SimpleFin fetch window (item `updated`).

Timestamps the **app** supplies from its own clock: `cookie_expires`, `auth_recheck_at`, `last_sync_at`, `revoked_at`, `last_used_at`, `expires_at`, `snapshot_date`, and the bound of the session purge (Q-SES-7).

## 4.3 Date and timestamp semantics

- `DATE` columns (calendar days, no zone): `transactions.date`, `investment_transactions.date`, `split_transactions.date`, `recurrings.splits_since`, `budgets/sections/categories.roll_over_start_date`, `securities.close_price_as_of`, `holdings.institution_price_as_of`, `cpi_snapshots.snapshot_date`. Read back as `YYYY-MM-DD` strings.
- `TIMESTAMPTZ` columns: every `updated`, `created_at`, `users.expiry`, `sessions.cookie_expires` / `auth_recheck_at`, `items.last_sync_at`, `snapshots.snapshot_date` (a timestamp despite the name), `api_keys.last_used_at` / `revoked_at` / `expires_at`, `rejected_categories.rejected_at`, `sharing_email_invitations.expires_at`. Stored with microseconds, read back as UTC ISO strings **truncated to milliseconds** (`pg/client.rs:241`).
- All date/time parameters travel as untyped text and the server parses them against the column type: `YYYY-MM-DD` for dates, ISO-8601 with `Z` for timestamps, and — from query strings — a bare date compared with a `TIMESTAMPTZ` (`updated >= '2026-11-01'`), which the server reads as midnight in the **session time zone**. The app never sets a session time zone, so the server default applies (the conformance harness runs the servers with `TZ=UTC` by default; the database's own zone is whatever the instance has).
- A `DATE` is compared with a timestamp in Q-SPL-8 (`t.date > NOW() - INTERVAL '1 week'`), which also promotes the date at session-zone midnight.
- Date arithmetic in SQL: `date - date` → integer days and `date ± integer` → date (Q-TXN-14), `EXTRACT(DAY FROM date)` (Q-TXN-11), `date::text` (Q-TXN-12/13), `MAX(date)` (Q-TXN-10), `to_char(MAX(timestamptz) AT TIME ZONE 'UTC', 'YYYY-MM-DD')` (Q-SNP-8).
- "Today" is computed in the app, sometimes as the UTC day (`utc_today`, `repo/transactions.rs:86`) and sometimes as the server process's local day (`get_date_string`), never by the database.

## 4.4 Numbers and money

- Money is `NUMERIC(15,2)` (`amount`, every `balances_*`, `institution_value`, `cost_basis`); prices, quantities and index values are `NUMERIC(15,6)`; `label_category_confidence` is `float8`; `sessions.cookie_*max_age` are `BIGINT`. There is no integer column otherwise.
- **In:** a JSON number is bound as `int4`, `int8` or `float8` (`pg/client.rs:81`) and the server converts it to the column's `NUMERIC(p,s)`, rounding to the scale (Postgres first renders the float to 15 significant decimal digits, then rounds half away from zero) and raising on overflow. The one array of amounts is cast explicitly (`$3::numeric[]`, Q-SPL-9).
- **Out:** `NUMERIC` and `BIGINT` are turned into 64-bit floats by parsing their decimal text (`pg/client.rs:259`), deliberately including float rounding beyond 2^53. The API therefore never sees a decimal type, but what is *stored* is exact decimal.
- **Exact decimal behaviour the SQL depends on:** `t2.amount = -t1.amount` (transfer matching, Q-TXN-14), `t.amount <> 0`, `ABS(t.amount) > $6::numeric`, `s.amount * SIGN(t.amount)` stored back into `NUMERIC(15,2)` (Q-SPL-9), `SIGN(t.amount) = SIGN($13::numeric)`, `SUM(score)::int`. `t.amount BETWEEN $5 AND $6` compares a `NUMERIC` column with `float8` parameters. `label_category_confidence = 1` / `= 1.0` / `> 0` / `< 1` are float comparisons that must treat a stored `1` as exactly 1.
- App-side money arithmetic is on floats with explicit rounding to cents (`repo/investment_transactions.rs:170`, `repo/recurrings.rs:383`).

## 4.5 JSONB

Columns: `raw` on `items`, `accounts`, `holdings`, `securities`, `institutions`, `transactions`, `investment_transactions` (the provider's payload, kept whole); `accounts.plan_fund_mappings` (object keyed by name); `capacities` on `budgets`, `sections`, `categories` (array); `charts.configuration` (any JSON); `recurrings.splits` (array, `NOT NULL DEFAULT '[]'`).

- Always written and read as a whole value; never patched in place. A builder `UPDATE` never writes `raw` (`pg/database.rs:170`); upserts do.
- The only operators are `->` and `->>` on `transactions.raw` (`raw->'personal_finance_category'->>'primary'`), compared with `=` and `LIKE 'TRANSFER%'`; a missing key must yield NULL, not an error.
- `SELECT DISTINCT s.*` (Q-SEC-1) needs equality over JSONB values.
- Postgres `jsonb` normalizes what it stores: object keys are de-duplicated and re-ordered (by length, then bytewise), whitespace is dropped. The app is exposed to that order in three places: `accounts.plan_fund_mappings` is returned as a list in stored key order (`pg/models/account.rs:107`), `charts.configuration` is re-serialized to text for the client (`pg/models/chart.rs:44`), and fields are read out of `securities.raw` (`pg/models/security.rs:69`). The conformance transcripts would change if the replacement kept insertion order instead.
- Binding: a JSON array or object bound to a JSONB column is sent as JSON text; the same JSON array bound to an array column is sent as an array literal — the driver decides from the parameter's type (`pg/client.rs:141`). A plain string bound to a JSONB column is parsed by the server as JSON text, so chart configuration is parsed in the app first.

## 4.6 Arrays

`TEXT[]` columns: `items.available_products` (nullable), `recurrings.transaction_ids` (`NOT NULL DEFAULT '{}'`), `api_keys.scopes` (`NOT NULL DEFAULT '{}'`). The function returns `UUID[]`.

Operations: whole-value read/write; `&&` overlap; `||` concatenation; `= ANY(column)` in a correlated subquery; `UNNEST(column || param)` with `DISTINCT` re-aggregated by `ARRAY(SELECT …)` (resulting element order unspecified); `unnest(function result)` as rows; `array_append`; `min()` over unnested elements cast to text. Array **parameters** are everywhere: `= ANY($n)` untyped, `::text[]`, `::uuid[]`, and three parallel arrays (`numeric[]`, `uuid[]`, `uuid[]`, possibly containing NULLs) unnested side by side into a derived table.

## 4.7 Text matching, collation, case folding

- No `COLLATE` clause appears anywhere, so every text comparison and sort uses the database's default collation. That reaches the API through `ORDER BY u.username` (Q-SHR-2), `ORDER BY invitee.username, …` (Q-SHR-3), `ORDER BY e.invitee_email, …` (Q-SHR-4), and reaches behaviour through `ORDER BY t.transaction_id` (Q-TXN-12, the detector's input order), `ORDER BY …, t1.transaction_id` with `LIMIT` (Q-TXN-14, which candidates survive the cap), `t1.transaction_id < t2.transaction_id` (which of two ids is "a"), and the keyset pagination on `VARCHAR` primary keys (`pk > $last ORDER BY pk`, which only needs `>` and `ORDER BY` to agree with each other). `docker-compose.yml` deploys `postgres:16-alpine` with default locale settings; the conformance schema test sorts with `COLLATE "C"` explicitly "so the order does not depend on the locale the Postgres instance was set up with".
- The app knows the database's order can disagree with its own: pairs are stored in an order computed in the app by UTF-16 code unit (`pg/models/transaction_pair.rs:77`), and `ct/detect_transfers.rs:152` notes "the candidate's own order comes from the database's collation, which need not agree".
- Case folding: `lower()` in SQL (`lower(email) = lower($1)`, the unique index, `lower(u.email) = e.invitee_email`) has to agree with the app's own lower-casing of invited addresses (`repo/sharing.rs:290`), or an invitation to a non-ASCII address would never match.
- `username`, ids, `merchant_key`, `key_hash` are compared case-sensitively and bytewise-equal.
- Regex: POSIX `~` and `SUBSTRING(… FROM '…')` with a capture group, `\d` and `[0-9]` classes (Q-TXN-7, Q-INV-6).
- Trigram similarity: `similarity(a, b) >= 0.5` (Q-TXN-11). pg_trgm lower-cases, splits into words on non-alphanumerics, pads each word and compares trigram sets (shared / union). The suggestion engine's results depend on these exact numbers; there is no index behind it, every confirmed transaction is scored.

## 4.8 Deletes and cascades

- **Database cascades exist but the app never triggers them.** `ON DELETE CASCADE` hangs off `transactions`, `categories` and `users` (section 1), yet no code path hard-deletes a transaction, a category or a user. They only matter for an operator deleting rows by hand (the README leaves removing a person's data "up to the operator"). `ON DELETE RESTRICT` likewise only stops manual deletes.
- **Foreign keys are relied on at insert time:** a section needs an existing budget, a category an existing section, a pair two existing transactions, a rejection an existing transaction and category, every owned row an existing user.
- **The cascades the app relies on are its own soft-delete fan-outs**, each a list of `UPDATE … SET is_deleted = TRUE` statements: item → accounts → (transactions, investment transactions, splits, pairs, balance snapshots, holding snapshots, holdings) (T3); accounts → the same (T2); transactions → splits and pairs (`repo/transactions.rs:357`); budget → sections → categories (T4); section → categories (T5); sharing break-up → pairs spanning the split (T8); hourly clean-ups for pairs with a deleted transaction (Q-PAR-9) and recurrings with no live member (Q-REC-12).
- Deliberately *not* cascaded: deleting holdings keeps their snapshots (`repo/holdings.rs:67`); deleting a budget or category keeps the labels that name it (`repo/budgets.rs:127`); deleting a category keeps its `rejected_categories` rows.
- Soft-deleted rows stay in unique constraints and are revived by upserts (pairs, recurrings, snapshots).

## 4.9 Row-level sharing and permission filters

There is no database-level access control. Every statement states its own scope, in one of four ways:

| scope | SQL | used for |
|---|---|---|
| sharing group | `user_id = ANY(sharing_user_ids($n))` — the bound value is the caller's own id; the function widens it inside the statement | every user-facing read and write on `items`, `accounts`, `holdings`, `transactions`, `investment_transactions`, `split_transactions`, `transaction_pairs`, `recurrings` (reads), `budgets`, `sections`, `categories`, `snapshots`, `charts`, `rejected_categories`; also inside joins (Q-TXN-11) and in Q-SEC-1 |
| one owner | `user_id = $n` | `api_keys`; `recurrings` writes; the auto-suggest and recurring-detection jobs; the plan-statement import; Q-SPL-6/7; Q-TXN-7 |
| explicit list | `user_id = ANY($n::uuid[])` with the group read once under the membership lock | transfer detection (Q-TXN-14, Q-PAR-9, Q-PAR-10), the break-up clean-up (Q-PAR-11) |
| none | — | global tables (`sessions`, `institutions`, `securities`, `cpi_snapshots`, security-price snapshots), the hourly "all items" read, the item-with-owner lookup, a few by-key writes inside a transaction that already established ownership (Q-REC-11, Q-REC-14, Q-REJ-3, Q-REJ-4) |

Properties the app depends on:

- Membership is resolved **per statement**, never cached in the session, so leaving a group takes effect at the next query in every process (`docs/SECURITY.md:106`).
- Rows keep their owner; sharing never rewrites `user_id`. A new row takes the owner of its parent, read with Q-SHR-5 (`get_row_owner`) or off the locked rows (Q-TXN-9 in `pair_transactions`), never the requester's id.
- The scope is ANDed into writes as well as reads (`UPDATE … WHERE pk = $1 AND user_id = ANY(sharing_user_ids($2))`), and "no row affected" is the not-found/forbidden answer.
- Upserts do not carry an owner predicate. Protection against a colliding id taking over another user's row is the per-table allowlist of columns a conflict may rewrite (items, holdings, account-balance and holding snapshots); transactions, investment transactions and accounts synced from a provider rewrite every supplied column, `user_id` included.

---

# 5. Migrations and lifecycle

## 5.1 Start-up sequence (`pg/initialize.rs:67`, identical in `ts/lib/postgres/initialize.ts`)

1. **Wait for the database:** try to check a connection out; on failure log and retry every 10 s, forever.
2. `CREATE EXTENSION IF NOT EXISTS pg_trgm`.
3. **Create tables:** for each of the 23 models in dependency order, `CREATE TABLE IF NOT EXISTS …` built from the model's column-definition strings and table constraints. Each is its own autocommit statement. An existing table is left exactly as it is.
4. `CREATE OR REPLACE FUNCTION sharing_user_ids …` (every start).
5. **Column migration** (`pg/migration.rs:203`), on one reserved connection:
   1. `SELECT pg_advisory_lock(6453604)` — session lock so that processes starting together (rolling deploys, clustered processes) take turns.
   2. `BEGIN`.
   3. For each table, read `information_schema.columns` (name, `data_type`, `udt_name`, nullability, default, in ordinal order). No rows means the table does not exist: skip.
   4. For each column **in the model**:
      - missing in the database → `ALTER TABLE t ADD COLUMN IF NOT EXISTS c <definition>`, with `PRIMARY KEY` stripped and, when the definition says `NOT NULL` without a `DEFAULT`, a default inferred from the type so existing rows get a value (`BOOLEAN` → `FALSE`; `INTEGER`/`BIGINT`/`SMALLINT` → `0`; `UUID` → `gen_random_uuid()`; `JSONB` → `'{}'::jsonb`; `JSON` → `'{}'::json`; any timestamp → `CURRENT_TIMESTAMP`; anything else → `''`). Inline `REFERENCES`, `UNIQUE` and `DEFAULT` in the definition are kept.
      - present → compare normalized types: the model's first word (`VARCHAR(n)`→`VARCHAR`, `TIMESTAMPTZ`/`TIMESTAMP`→`TIMESTAMP`, `INTEGER`/`INT`/`SERIAL`→`INTEGER`, `DECIMAL`/`NUMERIC`→`NUMERIC`, `REAL`/`FLOAT`/`DOUBLE PRECISION`→`FLOAT`, arrays as `ELEM[]`) against the catalog's (`character varying`→`VARCHAR`, `timestamp…`→`TIMESTAMP`, `ARRAY` + `_text`→`TEXT[]`, user-defined → its name). `VARCHAR`≈`TEXT`, `JSON`≈`JSONB`, `INTEGER`≈`INT4`, `FLOAT`≈`FLOAT8` are compatible. Length, precision, nullability, defaults and constraints are **not** compared.
   5. Columns in the database but not in the model produce a warning only.
   6. Any failed `ALTER` or any type mismatch is fatal: `ROLLBACK` and refuse to start with every problem listed. Otherwise `COMMIT`.
   7. `SELECT pg_advisory_unlock(6453604)` (always, errors ignored).
6. **Indexes**, after the migration so an index can cover a column just added: for each model index, when it is unique first run the duplicate check (Q-DDL-6) and refuse to start listing every duplicated key; then `CREATE [UNIQUE] INDEX IF NOT EXISTS …`.
7. **Seed users:** look up `admin` and `demo` by name and upsert each with the password from `ADMIN_PASSWORD` / `DEMO_PASSWORD` (default `budget`), re-hashing on every start.

What this scheme never does: drop, rename or retype a column; change a default, nullability or constraint on an existing table; add a table-level constraint or foreign key to an existing table; drop or rebuild an index whose definition changed; record a schema version (`VERSION = "6"` in `pg/initialize.rs:16` is an app data version reported by `GET /login`, not stored). Because it is additive only, the previous release keeps working against the new schema while both run (`docs/DATABASE.md`), which is the upgrade story the replacement has to keep.

## 5.2 Connection configuration

- Environment: `POSTGRES_HOST` (default `localhost`), `POSTGRES_PORT` (`5432`), `POSTGRES_USER` (`postgres`), `POSTGRES_PASSWORD`, `POSTGRES_DATABASE` (`budget`). No TLS (`NoTls`).
- One process-wide pool, at most **20** connections, connect timeout 2 s (`pg/client.rs:354`). A transaction or a migration pins one connection for its duration; the conformance README notes the database needs "`max_connections` of a few hundred since each started server keeps a pool of 20".
- No session parameters are set after connecting (no time zone, search path, application name, timeouts).
- Statements are not cached between calls (TypeScript `prepare: false`; Rust prepares per call with explicit parameter types), because statement texts vary without bound: `IN` lists sized by input, multi-row `VALUES` sized by batch, column sets that vary by payload (`ts/lib/postgres/client.ts:158`). A replacement API should not rely on a small fixed set of statement shapes.
- Shutdown: stop the schedulers, stop the HTTP server, close the pool (`start.rs:690`).

## 5.3 Multi-process assumptions

- Several copies of the app may run against one database, including two releases during a rolling deploy. Cross-process coordination that goes through the database: the migration lock, the advisory locks of section 3.4, sessions (shared store), sharing membership (resolved per statement).
- Not coordinated: the hourly job (every process runs it), in-memory rate limits, the BLS request budget, and real-time events — a write handled by one process notifies only streams connected to that process. With single sign-on on, each open stream re-reads its session every 60 s so that a session ended in another process closes it (`docs/SECURITY.md:58`).

## 5.4 Periodic and background work

| what | cadence | database work |
|---|---|---|
| scheduled sync (`ct/schedule.rs:241`) | once at start, then hourly; a tick is skipped while the previous run is still going (per process) | in order: all items (Q-ITM-2) and per item a provider sync plus Q-ITM-7; auto-suggest (per user: Q-TXN-6, Q-SPL-8, then Q-TXN-11 up to twice per target and Q-TXN-18 / Q-SPL-10); transfer detection (T17 per group); recurring detection (T16 per user); security-price refresh (Q-HLD-6, Q-SEC-3 and Q-SEC-4 for the benchmark, Q-SEC-2 per security, Q-SNP-9, Q-SNP-8, Q-SNP-11); CPI refresh (Q-CPI-1, Q-CPI-2) |
| session purge (`repo/session.rs:37`) | 60 s after start, then hourly | Q-SES-7 |
| SSO stream watch | every 60 s per open event stream, SSO only | Q-SES-1 |
| SSO session re-check | when a request arrives after `auth_recheck_at` (10 min) | Q-SES-4 or Q-SES-5 |
| price backfill | spawned after a plan-statement import or a new holding snapshot | Q-SNP-4 per security, then Q-SNP-11 per missing month |
| API-key "last used" | spawned per bearer request | Q-KEY-5 |
| event fan-out | queued after each successful mutation | Q-SHR-1 |

---

# 6. Scale hints

There is no production data in the repository, so sizes below are reasoned from the schema, the sync code and the comments, not measured. The app is a self-hosted personal-finance tracker: a handful of users (seeded `admin` and `demo`, plus household members who share data), each with a few connected institutions.

## 6.1 Which tables grow

| table | growth | typical row |
|---|---|---|
| `snapshots` | fastest by row count: one row per account whenever a sync sees a changed balance (at most one per day), one per holding per changed day plus a zero-quantity terminator when it disappears, one per held security per trading day, monthly back-filled prices, and the benchmark's daily history. Years × (accounts + holdings + securities) × ~250–365. | narrow: 19 columns, mostly NULL for any one kind; no JSON. Roughly 100–200 bytes. |
| `transactions` | every bank transaction: up to two years of history when an item is first linked, then daily. 10^3–10^5 per household. | 22 columns, one of them `raw`, the provider's whole transaction object. For Plaid that JSON is typically 1–3 KB, so `raw` is most of the table's bytes and is returned by every `SELECT *`. |
| `investment_transactions` | per trade/dividend; plan-statement imports add rows in bulk (chunks of 1000). | like transactions, with `raw`. |
| `transaction_pairs` | one per detected or manual transfer; suggestions accumulate (up to 500 new candidates per member per hourly run, in practice far fewer) and rejected/deleted rows stay forever as tombstones. | tiny (8 columns). |
| `split_transactions` | user-created or generated from recurring defaults; small. | small. |
| `recurrings` | dozens per user, but `transaction_ids` grows by one element per charge forever, and the row is rewritten each time. | an array of ids of roughly 40 characters each; hundreds of elements for an old weekly charge. |
| `rejected_categories` | one per rejected suggestion. | tiny. |
| `sessions` | one per sign-in, expiring after 7 days, purged hourly. Few rows, but **rewritten on every request**. | small. |
| `holdings`, `accounts`, `items`, `securities`, `institutions` | bounded by what is linked; tens to hundreds. All carry `raw`. | |
| `budgets`, `sections`, `categories`, `charts`, `api_keys`, sharing tables, `users` | tens. | `capacities` / `configuration` JSON, small. |
| `cpi_snapshots` | one row per month of one series; a few hundred at most (`repo/cpi_snapshots.rs:9`). | tiny. |

## 6.2 Hot paths

- **Every cookie-authenticated request** costs one session read (Q-SES-1) and one session write (Q-SES-2, or Q-SES-3) around the handler; a mutation adds Q-SHR-1 afterwards. Bearer requests cost Q-KEY-4, Q-USR-1 and an asynchronous Q-KEY-5 instead.
- **`sharing_user_ids($n)` is evaluated in almost every statement** (two index lookups on `sharing_members`). The model comment relies on it being evaluated once per statement and still letting the owner column's index be used (`pg/models/sharing.rs:100`).
- **Client sync** runs at app load, and again as a delta when a change event arrives: about a dozen GETs — `/transactions` (Q-TXN-2 + Q-INV-2), `/split-transactions` (Q-SPL-1), `/accounts` (Q-ACC-1 or Q-ACC-3, Q-HLD-1, Q-ITM-3), `/budgets` (Q-BSC-1 ×3), `/transfers` (Q-PAR-1 + Q-TXN-4), `/recurrings` (Q-REC-1 + Q-TXN-4), `/snapshots` (Q-SNP-1, Q-SNP-3, Q-SEC-2 per new security), `/charts`, `/institutions` (Q-INS-2), `/securities` (Q-SEC-1), `/inflation` (Q-CPI-1), `/sharing` (Q-SHR-2/3/4). Roughly 20–30 statements warm, each also paying the two session statements.
  - Cold sync streams whole tables in primary-key order, 500 rows per statement (served by the `(user_id, <pk>)` composites). Warm sync is `user_id … AND updated >= cursor` (served by the `(user_id, updated)` composites). The index comments in `pg/models/transaction.rs:47` say these two indexes exist precisely to keep the reads proportional to rows changed.
  - `/transfers` and `/recurrings` resolve transactions with one `IN` list holding every id (two per pair; every member of every recurring).
- **`POST /transaction`** (labelling, the most frequent write): Q-TXN-8, Q-BSC-3 twice, Q-TXN-17, then Q-REJ-1 and/or Q-REJ-2.
- **Provider sync** writes one upsert statement per row (accounts, holdings, securities, snapshots, transactions): a first sync of two years of history is thousands of round trips. Only institutions, CPI rows, plan-statement rows and sharing members are written in multi-row statements.

## 6.3 Expensive statements

- **Q-TXN-11 (auto-suggest)** scans all of a user's confirmed transactions and computes two trigram similarities per row, once or twice per unlabeled transaction or split of the last 7 days, hourly: cost ≈ unlabeled × confirmed. Its feeder Q-TXN-6 reads *every* unlabeled transaction of the user with `raw` and filters by date in the app.
- **Q-TXN-14 (transfer candidates)** is a self-join over the group's transactions; the SQL comment explains it is written so the planner can hash-join on `amount`. Two anti-joins against `transaction_pairs` per candidate pair, then sort and cut at 500 × members.
- **Q-TXN-12 (recurring candidates)** reads every settled transaction of the user hourly, with an anti-join.
- **Q-REC-12 / Q-PAR-9** are per-user clean-ups with correlated subqueries.
- **Q-SEC-2 in loops** (N+1 by design: "securities are few").
- Statement-size ceilings that exist today only because of the wire protocol: one bind parameter per `IN` element and per inserted value (at most 65 535 per statement), which bounds Q-TXN-4 and the 1000-row chunks of Q-INV-8.

---

# 7. Hardest things to replicate without SQL

Ranked by how much engine capability each one demands.

1. **The auto-suggest scoring query (Q-TXN-11).** A per-row scoring expression mixing trigram similarity, a JSON path lookup, sign tests, numeric and day-of-month bands; filter by score; group and sum per category; pick the top group; then a correlated aggregate over a join of two other tables using the same expression; then two more joins under the sharing scope. Needs either a small expression/aggregation engine in the database or a bit-exact reimplementation of `pg_trgm.similarity` and the rest in the client with a full scan of the user's confirmed history.
2. **Transfer-candidate detection (Q-TXN-14) and its guarded insert (Q-PAR-10).** A band self-join (exact negated decimal amount, ±7 days, different account, id ordering) with two more joins, two anti-joins, computed output columns, ordering by an expression and a limit; then insert-unless-exists with two anti-joins, per candidate, under savepoints, where a unique violation is an expected outcome.
3. **The concurrency protocol (section 3).** READ COMMITTED semantics with blocking row locks and predicate re-check (`FOR SHARE` against concurrent soft deletes, `FOR UPDATE` against cascades), transaction-scoped advisory locks in shared and exclusive modes with keys computed from the sharing group, a session-scoped advisory lock, savepoints, long multi-statement transactions that read their own writes, and Postgres's poisoned-transaction behaviour that some call sites silently lean on.
4. **Upserts with logic.** Conflict targets other than the primary key (two- and three-column unique keys, a composite primary key, and "any unique key including a partial expression index"), `SET` clauses that read both the existing and the proposed row (`CASE`, array union, conditional rename driven by a parameter), `DO UPDATE … WHERE` that can decline to write **and return nothing**, `RETURNING` of the surviving row, and `INSERT … SELECT … ON CONFLICT`.
5. **Sharing scope evaluated inside every statement.** `user_id = ANY(sharing_user_ids($n))` on reads, writes, joins and sub-selects, always current within the statement, combined with set-based maintenance of the group (`DELETE … USING` two aliases of the membership table, `UPDATE … FROM` two aliases of `transactions` with `(a.user_id = $2) <> (b.user_id = $2)`, a `UNION ALL` across the two invitation tables with a scalar subquery on `lower(email)` and the server clock).
6. **Set-based label and split propagation for recurrings (Q-TXN-19, Q-SPL-9).** One `UPDATE` with NULL-safe comparisons, a four-branch predicate over a float state column and a "would this change anything" guard; one `INSERT … SELECT` that cross-joins transactions with three parallel array parameters, computes signed amounts in exact decimal, derives a state column with `CASE`, anti-joins against existing splits and generates a UUID per produced row.
7. **Array-valued membership (`recurrings.transaction_ids`).** Overlap probes, append, de-duplicating union inside an upsert's `SET`, and a correlated `NOT EXISTS … = ANY(array column)` against another table.
8. **The dynamic builder contract (section 2.0).** Schemaless-looking partial writes over a typed schema: absent key = untouched/default, `null` = NULL; per-call column allowlists on conflict; `updated` stamped with server (transaction-start) time on every write; `RETURNING *` rows validated against the model; affected-row counts mapped to 200/304/404; generic operations parameterized by table and column names (`get_row_owner`, soft delete by related column).
9. **The delta-sync contract.** `updated >= cursor` ranges per owner group including tombstones, ordered keyset streaming in 500-row pages over text keys with no shared snapshot, composite indexes that make both cheap, server-side parsing of client-supplied date/timestamp text (and the pinned 500 when it cannot be parsed), millisecond-truncated timestamps on the way out.
10. **Exact decimals behind a float API.** `NUMERIC(15,2)` / `(15,6)` storage with rounding on write and overflow errors, exact equality and sign arithmetic in queries, but floats in and out of the driver.
11. **Self-migrating schema.** Create-if-missing tables from model text, additive column migration from a catalog diff with type-compatibility rules and inferred defaults, transactional DDL, a duplicate pre-check before unique (partial, expression) indexes, all under a cross-process lock and compatible with two releases running at once.
12. **Locale-dependent text behaviour.** Default-collation ordering visible in API output and in detection inputs, `lower()` agreeing with the app's case folding, POSIX regex with capture, and jsonb's key ordering leaking into responses.
