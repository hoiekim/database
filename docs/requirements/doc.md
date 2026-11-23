# `doc` — database usage inventory (requirements for a Postgres replacement)

Source: `the doc repository` at commit `6e24c0d`. Read in full: all five migrations, `src/server-rs/src/db.rs` and
every Rust module that issues SQL (33 files), the TypeScript twins under `src/server` for cross-checking, and the
API-suite harness where it reveals a database behaviour the app depends on.

Conventions used below

- `file:line` references are to the **Rust** server, relative to `src/server-rs/src/` (e.g. `pages.rs:304`).
  Migrations are relative to `src/server/db/migrations/`.
- SQL is quoted literally but trimmed. `$n` stands for a bound value (the Rust `sql!` macro writes `{}`);
  `«name»` stands for a spliced SQL fragment defined elsewhere in the catalogue.
- "Shape" = one distinct statement form. Identical shapes used at several call sites are listed once with all
  their references. Shapes are numbered `Q1 … Q191` (totals in §2.26); the seven fragments spliced into many
  of them are `F1 … F7` (start of §2); transactions are `T1 … T25` (§3.2).

## 0. Which engine, and how it is used

**PostgreSQL only.** There is no SQLite, no Redis, no second store.

| | Rust server (`src/server-rs`) — primary | Bun/TS server (`src/server`) |
|---|---|---|
| Driver | `tokio-postgres 0.7` + `deadpool-postgres 0.14` (`db.rs:16-21`) | `Bun.SQL` tagged templates (`db/index.ts:1`) |
| TLS to DB | **None** (`NoTls`, `db.rs:619`); `sslmode=require` refuses to start | Supported by Bun |
| Parameters | Every value is sent as **text of unknown type** and Postgres coerces it (`TextParam`, `db.rs:302-326`, `query_typed` with `Type::UNKNOWN`, `db.rs:649`). Queries therefore carry explicit casts (`$1::uuid`, `$1::jsonb`, `$1::uuid[]`, `$1::int`, `$1::timestamptz`, `$1::numeric`, `$1::interval`, `$1::float8`, `$1::boolean`, `$1::text`). Arrays are sent as array literals `{"a","b"}` (`db.rs:188`). JSON is sent as text and cast `::jsonb`. | Bun infers types; arrays via `sql.array(v, "uuid"\|"text")`; objects are bound as JSON |
| Result decoding | Binary format, decoded by column type into `Cell::{Null,Bool,Int,Float,Text,Json,Time,Array}` (`db.rs:333-382`). Supported column types: `void, bool, int2, int4, int8, oid, float4, float8, text/varchar/bpchar/name/unknown, uuid, json, jsonb, timestamptz/timestamp`, and arrays of those. **`numeric`, `bytea`, `tsvector`, `interval`, `date` are never selected** (they would be a decode error). | Bun's decoding |
| Timestamps | Read as microseconds since epoch, **truncated to milliseconds** when returned to the API (`micros_to_iso`, `db.rs:414`) | `Date` (milliseconds) |
| Query style | Dynamic SQL assembled from nested fragments (`Sql::format`/`splice`, `db.rs:215-280`); no ORM, no prepared-statement cache (unnamed statements) | Same, with tagged-template fragments |
| Transactions | `db::begin(async \|tx\| …)` → `BEGIN` … `COMMIT`, `ROLLBACK` when the closure returns `Err` (`db.rs:685-703`) | `sql.begin(async (tx) => …)` |

The two servers issue **the same SQL**. I diffed every statement of the 33 Rust files that issue SQL against
its TS twin after normalising whitespace and placeholders; nothing differs in meaning. The complete list of
differences:

- Rust adds the explicit parameter casts described above; TS lets the driver type parameters.
- Rust aliases bare `SELECT 1` (`AS one` / `AS taken` / `AS done`) and `author_id AS id`, `user_id AS id` in two
  notification helper queries; TS writes `NULL AS content` where Rust writes `NULL::jsonb AS content`.
- TS binds an empty object for `notifications.data` where Rust writes the literal `'{}'::jsonb`.
- TS has two spelled-out queries for backlinks/outgoing links where Rust splices the column names (Q126).
- Error detection: TS reads `error.errno` (`"23505"`, `"40P01"`), Rust the SQLSTATE of the driver error.
- Reading: Rust re-normalises numbers inside JSON to JavaScript's formatting and replaces lone surrogates in
  text parameters with U+FFFD; Bun does both implicitly.
- The Rust server cannot use TLS to the database; its pool has no acquire timeout; TS closes its pool with a 5 s
  timeout at shutdown.

Both servers are designed to run **side by side on one database** (rolling deploys; the API suite starts up to
three copies on one database), so the database is the only shared state between processes.

---

## 1. Schema

26 tables: 25 created by the migrations plus `schema_migrations`. No sequences, no serial/identity columns, no
triggers, no enum types (enumerations are `text` + `CHECK`), no views, no partitioning, no `bytea`.

- **Extension:** `pg_trgm` (`001_initial.sql:1`) — used only for one GIN trigram index.
- **Indexes:** 34 `CREATE INDEX` statements (12 of them unique; 9 partial; 6 on expressions; 2 GIN) plus the
  primary keys and 6 column-level `UNIQUE` constraints. All are listed per table below.
- **Built-ins relied on:** `gen_random_uuid()` (uuid v4 defaults), `now()` (transaction start time — see §4.4),
  `to_tsvector`/`setweight`/`to_tsquery`/`ts_rank` (full-text), jsonb operators and functions (§4.6).
- **User-defined SQL functions:** five (§1.27).
- **Collation:** `position` columns are `COLLATE "C"` (bytewise); everything else uses the database default
  collation/ctype (§4.9).
- Every `id uuid PRIMARY KEY DEFAULT gen_random_uuid()` is **server-generated unless the app supplies one**; the
  app supplies its own v4 UUIDs for uploaded page files and for the pages and files copied by "duplicate page"
  (so that references between the copies can be written before they are inserted). The app also
  generates all short public ids itself (`common/ids.rs`): `pages.slug_id` (10 chars of `[0-9A-Za-z]`),
  `shares.key` (10 chars of `[0-9a-z]`), `base_properties.id` (`p` + 9 chars `[0-9a-z]`), secret tokens
  (32 random bytes, base64url; only their SHA-256 hex is stored).
- Every `created_at`/`updated_at` default is `now()`; **`updated_at` is never maintained by a trigger** — each
  UPDATE sets it explicitly (or deliberately does not: moving a page does not touch `updated_at`).

Legend: **G** = server-generated default; `→ T(c) CASCADE|SET NULL` = foreign key with its `ON DELETE` action
(no FK declares `ON UPDATE`; primary keys are never updated).

### 1.1 `workspace` — singleton row (`001_initial.sql:3-13,144-145`)

| column | type | null | default |
|---|---|---|---|
| `id` | smallint **PK** | no | `1`, `CHECK (id = 1)` |
| `name` | text | no | `'My workspace'` |
| `description` | text | yes | |
| `logo` | text | yes | (image file name `uuid.ext`) |
| `email_domains` | **text[]** | no | `'{}'` |
| `settings` | **jsonb** | no | `'{}'` — keys used: `sharingDisabled` (bool), `trashRetentionDays` (int 1–3650), `defaultPageEditMode` (`read`/`edit`) |
| `default_space_id` | uuid | yes | → `spaces(id)` **SET NULL** (added by `ALTER TABLE` after `spaces` exists) |
| `created_at`, `updated_at` | timestamptz | no | `now()` G |

### 1.2 `users` (`001_initial.sql:15-31`, `005_sso.sql:4-8`)

| column | type | null | default |
|---|---|---|---|
| `id` | uuid **PK** | no | `gen_random_uuid()` G |
| `email` | text | no | |
| `name` | text | no | `''` |
| `password_hash` | text | yes | (argon2id PHC string; NULL for SSO-created users) |
| `avatar` | text | yes | |
| `role` | text | no | `'member'`, `CHECK (role IN ('owner','admin','member'))` |
| `locale`, `timezone` | text | yes | |
| `preferences` | **jsonb** | no | `'{}'` |
| `invited_by` | uuid | yes | → `users(id)` **SET NULL** (self-reference) |
| `last_login_at`, `deactivated_at` | timestamptz | yes | |
| `created_at`, `updated_at` | timestamptz | no | `now()` G |
| `auth_sub` | text | yes | (005) identity-provider subject |
| `email_from_provider` | boolean | no | `false` (005) |

Indexes: `UNIQUE users_email_key (lower(email))` — **expression** unique index; `UNIQUE users_auth_sub_key (auth_sub)`
(plain unique: many NULLs allowed, non-NULL values unique).

### 1.3 `sessions` (`001_initial.sql:33-43`, `005_sso.sql:10-14`)

| column | type | null | default |
|---|---|---|---|
| `id` | uuid **PK** | no | G |
| `user_id` | uuid | no | → `users(id)` **CASCADE** |
| `token_hash` | text | no | **UNIQUE** (SHA-256 hex of the cookie) |
| `user_agent`, `ip` | text | yes | |
| `created_at`, `last_seen_at` | timestamptz | no | `now()` G |
| `expires_at` | timestamptz | no | (app-computed: now + 30 days, slid forward) |
| `auth_sid`, `auth_refresh_token` | text | yes | (005) |
| `auth_recheck_at` | timestamptz | yes | (005) |

Indexes: `sessions_user_idx (user_id)`; `sessions_auth_sid_idx (auth_sid) WHERE auth_sid IS NOT NULL` (**partial**).

### 1.4 `user_tokens` (`001_initial.sql:45-52`)

`id uuid PK G`, `user_id uuid NOT NULL → users CASCADE`, `purpose text NOT NULL` (only `'password-reset'` is
used), `token_hash text NOT NULL UNIQUE`, `expires_at timestamptz NOT NULL`, `created_at timestamptz NOT NULL DEFAULT now()`.

### 1.5 `groups` (`001_initial.sql:54-63`)

`id uuid PK G`, `name text NOT NULL`, `description text`, `is_default boolean NOT NULL DEFAULT false`,
`created_by uuid → users SET NULL`, `created_at`, `updated_at`.
Index: `UNIQUE groups_name_key (lower(name))` (expression).

### 1.6 `group_members` (`001_initial.sql:65-71`)

`group_id uuid NOT NULL → groups CASCADE`, `user_id uuid NOT NULL → users CASCADE`, `created_at`.
**PK `(group_id, user_id)`** (composite, no surrogate id). Index: `group_members_user_idx (user_id)`.

### 1.7 `invitations` (`001_initial.sql:73-82`)

`id uuid PK G`, `email text NOT NULL`, `role text NOT NULL DEFAULT 'member' CHECK (role IN ('admin','member'))`,
`group_ids uuid[] NOT NULL DEFAULT '{}'` (**array of ids, no FK**), `token_hash text NOT NULL` (not unique),
`invited_by uuid → users SET NULL`, `created_at` (doubles as "sent at": reset on resend; TTL 14 days is computed
from it).
Index: `UNIQUE invitations_email_key (lower(email))` (expression; it is an `ON CONFLICT` target).

### 1.8 `spaces` (`001_initial.sql:84-95`)

`id uuid PK G`, `name text NOT NULL`, `slug text NOT NULL`, `description text`, `icon text`,
`settings jsonb NOT NULL DEFAULT '{}'` (key used: `sharingDisabled`), `created_by uuid → users SET NULL`,
`created_at`, `updated_at`.
Index: `UNIQUE spaces_slug_key (lower(slug))` (expression).

### 1.9 `space_members` (`001_initial.sql:97-110`)

| column | type | null | notes |
|---|---|---|---|
| `id` | uuid **PK** | no | G |
| `space_id` | uuid | no | → `spaces` **CASCADE** |
| `user_id` | uuid | yes | → `users` **CASCADE** |
| `group_id` | uuid | yes | → `groups` **CASCADE** |
| `role` | text | no | `CHECK (role IN ('admin','writer','reader'))` |
| `added_by` | uuid | yes | → `users` **SET NULL** |
| `created_at` | timestamptz | no | G |

`CHECK ((user_id IS NULL) <> (group_id IS NULL))` — exactly one of user/group.
Indexes: `UNIQUE space_members_user_key (space_id, user_id) WHERE user_id IS NOT NULL` and
`UNIQUE space_members_group_key (space_id, group_id) WHERE group_id IS NOT NULL` (**partial unique**, both are
`ON CONFLICT` targets); `space_members_user_idx (user_id)`; `space_members_group_idx (group_id)`.

### 1.10 `pages` — the page tree (`001_initial.sql:112-142`, `002_bases.sql:3`)

| column | type | null | default / notes |
|---|---|---|---|
| `id` | uuid **PK** | no | G (app-supplied when duplicating) |
| `slug_id` | text | no | **UNIQUE**; app-generated |
| `space_id` | uuid | no | → `spaces` **CASCADE** |
| `parent_id` | uuid | yes | → `pages(id)` **CASCADE** (self-reference = the tree; NULL = root) |
| `position` | text **COLLATE "C"** | no | fractional-index key over `[0-9A-Za-z]`, ordered bytewise |
| `title`, `icon`, `cover` | text | yes | |
| `content` | **jsonb** | yes | the whole ProseMirror-style document tree |
| `text_content` | text | yes | plain text of the document, derived by the app |
| `doc_version` | **bigint** | no | `0`; number of collaborative steps applied (optimistic-concurrency token) |
| `search` | **tsvector GENERATED ALWAYS … STORED** | | see DDL below |
| `creator_id`, `last_editor_id` | uuid | yes | → `users` **SET NULL** |
| `contributor_ids` | **uuid[]** | no | `'{}'` (no FK; set-union maintained by the app) |
| `created_at`, `updated_at` | timestamptz | no | G |
| `deleted_at` | timestamptz | yes | soft delete (trash) |
| `deleted_by` | uuid | yes | → `users` **SET NULL** |
| `is_base` | boolean | no | `false` (002) |

```sql
search tsvector GENERATED ALWAYS AS (
  setweight(to_tsvector('simple', coalesce(title, '')), 'A') ||
  setweight(to_tsvector('simple', left(coalesce(text_content, ''), 1000000)), 'B')
) STORED
```

Indexes:

| index | definition | used by |
|---|---|---|
| (unique) | `slug_id` | lookup by slug |
| `pages_tree_idx` | btree `(space_id, parent_id, position)` | children listing in order, min/max sibling position |
| `pages_parent_idx` | btree `(parent_id)` | `has_children` EXISTS, recursive descent, FK cascade |
| `pages_search_idx` | **GIN `(search)`** | `search @@ tsquery` |
| `pages_title_trgm_idx` | **GIN `(lower(title) gin_trgm_ops)`** | intended for title substring search. Note: every query writes `lower(coalesce(p.title, '')) LIKE …`, which is a different expression, so this index is probably never chosen. |
| `pages_updated_idx` | btree `(updated_at DESC) WHERE deleted_at IS NULL` (partial, descending) | recent pages |
| `pages_deleted_idx` | btree `(space_id, deleted_at) WHERE deleted_at IS NOT NULL` (partial) | trash list, purge |

### 1.11 `page_versions` — history snapshots (`001_initial.sql:147-157`)

`id uuid PK G`, `page_id uuid NOT NULL → pages CASCADE`, `title text`, `icon text`, `content jsonb` (full copy of
the document), `editor_id uuid → users SET NULL`, `contributor_ids uuid[] NOT NULL DEFAULT '{}'`,
`created_at timestamptz NOT NULL DEFAULT now()`.
Index: `page_versions_page_idx (page_id, created_at DESC)`.

### 1.12 `comments` (`001_initial.sql:159-173`)

`id uuid PK G`, `page_id uuid NOT NULL → pages CASCADE`, `parent_id uuid → comments(id) CASCADE` (self-reference;
the app flattens replies to one level), `author_id uuid → users SET NULL`, `body jsonb NOT NULL` (a small
document), `quote text`, `anchored boolean NOT NULL DEFAULT false`, `created_at`, `updated_at`,
`edited_at timestamptz`, `resolved_at timestamptz`, `resolved_by uuid → users SET NULL`.
Index: `comments_page_idx (page_id, created_at)`.

### 1.13 `files` — metadata of uploads; bytes live on disk or S3 (`001_initial.sql:175-188`)

`id uuid PK G` (app-supplied for page files), `purpose text NOT NULL DEFAULT 'page' CHECK (purpose IN
('page','avatar','workspace-logo','space-icon'))`, `page_id uuid → pages` **SET NULL**, `space_id uuid → spaces`
**SET NULL**, `uploader_id uuid → users SET NULL`, `name text NOT NULL`, `mime text NOT NULL DEFAULT
'application/octet-stream'`, `size bigint NOT NULL DEFAULT 0`, `storage_key text NOT NULL` (not unique, not
indexed, but looked up and deleted by), `created_at`, `updated_at`.
Index: `files_page_idx (page_id)`.

### 1.14 `shares` — public links (`001_initial.sql:190-199`)

`id uuid PK G`, `key text NOT NULL UNIQUE`, `page_id uuid NOT NULL UNIQUE → pages CASCADE` (one share per page;
`ON CONFLICT (page_id)` target), `include_children boolean NOT NULL DEFAULT false`,
`search_indexing boolean NOT NULL DEFAULT false`, `created_by uuid → users SET NULL`, `created_at`, `updated_at`.

### 1.15 `favorites` (`001_initial.sql:201-210`)

`id uuid PK G`, `user_id uuid NOT NULL → users CASCADE`, `page_id uuid → pages CASCADE`,
`space_id uuid → spaces CASCADE`, `created_at`. `CHECK ((page_id IS NULL) <> (space_id IS NULL))`.
Indexes: `UNIQUE favorites_page_key (user_id, page_id) WHERE page_id IS NOT NULL`,
`UNIQUE favorites_space_key (user_id, space_id) WHERE space_id IS NOT NULL` (partial unique, `ON CONFLICT` targets).

### 1.16 `watchers` (`001_initial.sql:212-223`)

Same shape as `favorites` (`user_id`, `page_id` xor `space_id`, same CHECK and the same two partial unique
indexes `watchers_page_key` / `watchers_space_key`), plus `watchers_page_idx (page_id)` and
`watchers_space_idx (space_id)`.

### 1.17 `labels` / 1.18 `page_labels` (`001_initial.sql:225-237`)

`labels`: `id uuid PK G`, `name text NOT NULL UNIQUE` (case-sensitive; the app lower-cases and dash-joins before
writing), `created_at`.
`page_labels`: `page_id → pages CASCADE`, `label_id → labels CASCADE`, `created_at`, **PK `(page_id, label_id)`**;
index `page_labels_label_idx (label_id)`.

### 1.19 `page_links` — link graph for backlinks (`001_initial.sql:239-244`)

`source_id uuid NOT NULL → pages CASCADE`, `target_id uuid NOT NULL → pages CASCADE`, **PK `(source_id, target_id)`**;
index `page_links_target_idx (target_id)`. Fully derived from `pages.content`; rebuilt on every save.

### 1.20 `synced_blocks` — transcluded blocks (`001_initial.sql:246-253`)

`page_id uuid NOT NULL → pages CASCADE`, `block_id text NOT NULL`, `content jsonb NOT NULL`,
`updated_at timestamptz NOT NULL DEFAULT now()`, **PK `(page_id, block_id)`**; index `synced_blocks_block_idx (block_id)`.
Derived from `pages.content`; rebuilt on every save.

### 1.21 `notifications` (`001_initial.sql:255-267`)

`id uuid PK G`, `user_id uuid NOT NULL → users CASCADE`, `kind text NOT NULL` (`comment.created`, `comment.reply`,
`comment.mention`, `page.mention`, `page.updated`), `actor_id uuid → users SET NULL`,
`page_id uuid → pages CASCADE`, `comment_id uuid → comments CASCADE`, `data jsonb NOT NULL DEFAULT '{}'` (always
`{}` today), `created_at`, `read_at timestamptz`, `emailed_at timestamptz`.
Index: `notifications_user_idx (user_id, created_at DESC)`.

### 1.22 `base_properties` — columns of a "base" (typed table page) (`002_bases.sql:5-19`)

`page_id uuid NOT NULL → pages CASCADE`, `id text NOT NULL` (short app id; row cells are keyed by it),
`name text NOT NULL`, `type text NOT NULL` (17 types, validated by the app, no CHECK), `options jsonb NOT NULL
DEFAULT '{}'` (choices, formula source/resultType/dependencies, number format …; ≤ 256 KiB),
`is_primary boolean NOT NULL DEFAULT false`, `position text NOT NULL COLLATE "C"`, `created_at`, `updated_at`.
**PK `(page_id, id)`**. Index: `UNIQUE base_properties_name_key (page_id, lower(btrim(name)))` — **expression
unique** (its violation is caught, §3.4).

### 1.23 `base_rows` (`002_bases.sql:21-31`)

`id uuid PK G`, `page_id uuid NOT NULL → pages CASCADE`, `cells jsonb NOT NULL DEFAULT '{}'` (object: property id →
value; values are strings, numbers, booleans, arrays of strings, arrays of file objects),
`position text NOT NULL COLLATE "C"`, `creator_id uuid → users SET NULL`, `last_editor_id uuid → users SET NULL`,
`created_at`, `updated_at`.
Index: `base_rows_order_idx (page_id, position, id)` — the keyset-pagination order.

### 1.24 `base_views` (`002_bases.sql:33-44`)

`id uuid PK G`, `page_id uuid NOT NULL → pages CASCADE`, `name text NOT NULL`,
`type text NOT NULL CHECK (type IN ('table','kanban','calendar'))`, `config jsonb NOT NULL DEFAULT '{}'` (stored
filter tree, sorts, hidden/ordered property ids, widths …), `position text NOT NULL COLLATE "C"`,
`creator_id uuid → users SET NULL`, `created_at`, `updated_at`.
Index: `base_views_page_idx (page_id, position)`.

### 1.25 `realtime_leader` — singleton lease row (`004_realtime_leader.sql`)

```sql
CREATE TABLE realtime_leader (
  id int PRIMARY KEY DEFAULT 1 CHECK (id = 1),
  holder text NOT NULL,
  expires_at timestamptz NOT NULL
);
```

### 1.26 `schema_migrations` (`db.rs:717`)

```sql
CREATE TABLE IF NOT EXISTS schema_migrations (name text PRIMARY KEY, applied_at timestamptz NOT NULL DEFAULT now())
```

### 1.27 SQL functions (`002_bases.sql:49-94`, `003_cell_time_zone.sql`)

All take a `jsonb` cell value. "Anything that does not convert cleanly reads as NULL instead of failing the query."

```sql
-- number: JSON numbers, or strings that look like a decimal/exponent number
cell_number(v jsonb) RETURNS numeric IMMUTABLE :=
  CASE jsonb_typeof(v)
    WHEN 'number' THEN (v #>> '{}')::numeric
    WHEN 'string' THEN CASE WHEN (v #>> '{}') ~ '^\s*[-+]?(\d+\.?\d*|\.\d+)([eE][-+]?\d+)?\s*$' THEN (v #>> '{}')::numeric END
  END

-- time: only strings. A bare YYYY-MM-DD is local midnight in the viewer's zone (offset minutes east of UTC);
-- anything else is whatever `text::timestamptz` accepts; any error -> NULL  (plpgsql, STABLE, EXCEPTION WHEN others)
cell_time(v jsonb, offset_minutes integer) RETURNS timestamptz :=
  s := btrim(v #>> '{}');
  IF s ~ '^\d{4}-\d{2}-\d{2}$' THEN RETURN (s || 'T00:00:00Z')::timestamptz - make_interval(mins => offset_minutes);
  RETURN s::timestamptz;

-- flag: booleans; strings true/yes/on/1 and false/no/off/0/'' (trimmed, lower-cased); numbers <> 0
cell_flag(v jsonb) RETURNS boolean IMMUTABLE

-- list: arrays stay arrays, a single string becomes a one-item array, everything else NULL
cell_list(v jsonb) RETURNS jsonb IMMUTABLE :=
  CASE jsonb_typeof(v) WHEN 'array' THEN v WHEN 'string' THEN jsonb_build_array(v) END

-- patch: a JSON null removes the key, anything else sets it
apply_cell_patch(cells jsonb, patch jsonb) RETURNS jsonb IMMUTABLE :=
  coalesce((SELECT jsonb_object_agg(key, value) FROM (
      SELECT key, value FROM jsonb_each(cells) WHERE NOT patch ? key
      UNION ALL
      SELECT key, value FROM jsonb_each(patch) WHERE jsonb_typeof(value) <> 'null') merged), '{}'::jsonb)
```

(`002` first created a one-argument `cell_time(jsonb)`; `003` adds the two-argument form and drops the old one.)

### 1.28 Foreign-key summary (the cascade graph)

50 foreign keys: 30 `ON DELETE CASCADE`, 20 `ON DELETE SET NULL`; none deferrable, none with `ON UPDATE`.

```
users      <- sessions, user_tokens, group_members, space_members.user_id, favorites, watchers, notifications.user_id   CASCADE
users      <- users.invited_by, groups.created_by, invitations.invited_by, spaces.created_by, space_members.added_by,
              pages.creator_id/last_editor_id/deleted_by, page_versions.editor_id, comments.author_id/resolved_by,
              files.uploader_id, shares.created_by, notifications.actor_id, base_rows.creator_id/last_editor_id,
              base_views.creator_id                                                                                    SET NULL
groups     <- group_members, space_members.group_id                                                                    CASCADE
spaces     <- space_members, pages, favorites.space_id, watchers.space_id                                              CASCADE
spaces     <- workspace.default_space_id, files.space_id                                                               SET NULL
pages      <- pages.parent_id (self), page_versions, comments, shares, favorites.page_id, watchers.page_id,
              page_labels, page_links.source_id, page_links.target_id, synced_blocks, notifications.page_id,
              base_properties, base_rows, base_views                                                                   CASCADE
pages      <- files.page_id                                                                                            SET NULL
comments   <- comments.parent_id (self), notifications.comment_id                                                      CASCADE
labels     <- page_labels                                                                                              CASCADE
```

Not foreign keys (plain arrays or JSON that hold ids, tolerated when dangling): `pages.contributor_ids`,
`page_versions.contributor_ids`, `invitations.group_ids`, user/page/file ids inside `base_rows.cells`, page ids
and file ids inside `pages.content`.

---
## 2. Query catalogue

General observations before the list:

- **The app never reads an affected-row count.** Wherever it needs to know whether or what a write changed it
  uses `RETURNING` (Rust `execute` discards everything; `first`/`query` read rows). A typed API needs
  "update/delete returning the affected rows (or the previous value)" rather than counts.
- Timestamps written by the database (`now()`) and timestamps computed by the app and sent as text
  (`expires_at`, `auth_recheck_at`) are mixed in the same rows.
- Seven **reusable fragments** are spliced into a large share of the read queries:

  **F1 `tree_columns`** (`pages.rs:22`) — the columns of a tree item, with a correlated EXISTS per row:
  ```sql
  p.id, p.slug_id, p.title, p.icon, p.parent_id, p.space_id, p.position, p.is_base,
  EXISTS (SELECT 1 FROM pages c WHERE c.parent_id = p.id AND c.deleted_at IS NULL) AS has_children
  ```
  **F2 `page_columns`** (`pages.rs:30`) — everything but `content`/`text_content`/`search`:
  ```sql
  p.id, p.slug_id, p.space_id, p.parent_id, p.position, p.title, p.icon, p.cover, p.doc_version,
  p.creator_id, p.last_editor_id, p.contributor_ids::text[] AS contributor_ids, p.created_at, p.updated_at,
  p.deleted_at, p.deleted_by, p.is_base, EXISTS (…same as F1…) AS has_children
  ```
  **F3 `role_rank(space_col, user)`** (`permissions.rs:47`) — the caller's effective role in a space: the
  highest of a direct grant and grants through any of the user's groups (scalar correlated subquery, NULL = no access):
  ```sql
  (SELECT max(CASE sm.role WHEN 'admin' THEN 3 WHEN 'writer' THEN 2 ELSE 1 END)
   FROM space_members sm
   WHERE sm.space_id = «space_col»
     AND (sm.user_id = $u OR sm.group_id IN (SELECT gm.group_id FROM group_members gm WHERE gm.user_id = $u)))
  ```
  **F4 `readable_space_ids(user)`** (`permissions.rs:63`) — used as `p.space_id IN «F4»` (semi-join) in every
  cross-space listing:
  ```sql
  (SELECT sm.space_id FROM space_members sm
   WHERE sm.user_id = $u OR sm.group_id IN (SELECT gm.group_id FROM group_members gm WHERE gm.user_id = $u))
  ```
  **F5 `space_ref_json(s)`** (`spaces.rs:43`) and **F6 `user_ref_json(u)`** (`users.rs:155`) — the server builds
  nested JSON objects in SQL from a joined row:
  ```sql
  json_build_object('id', s.id, 'name', s.name, 'slug', s.slug,
                    'iconUrl', CASE WHEN s.icon IS NULL THEN NULL ELSE '/api/images/' || s.icon END)
  CASE WHEN u.id IS NULL THEN NULL ELSE json_build_object('id', u.id, 'name', u.name,
       'avatarUrl', CASE WHEN u.avatar IS NULL THEN NULL ELSE '/api/images/' || u.avatar END) END
  ```
  **F7 `sharing_enabled(s)`** (`shares.rs:63`) — a predicate reading two JSON settings:
  ```sql
  NOT coalesce((SELECT (settings->>'sharingDisabled')::boolean FROM workspace WHERE id = 1), false)
  AND NOT coalesce((s.settings->>'sharingDisabled')::boolean, false)
  ```

### 2.1 `schema_migrations`, health, locks

- **Q1** `db.rs:717` — `CREATE TABLE IF NOT EXISTS schema_migrations (…)` (idempotent DDL at every start).
- **Q2** `db.rs:721` — `SELECT name FROM schema_migrations` (full scan).
- **Q3** `db.rs:729` — `SELECT 1 AS done FROM schema_migrations WHERE name = $1` (existence, inside the lock).
- **Q4** `db.rs:733` — `INSERT INTO schema_migrations (name) VALUES ($1)`.
- **Q5** `db.rs:732` — `tx.batch(text)`: a whole migration file as one multi-statement simple-protocol batch
  inside the transaction (transactional DDL).
- **Q6** advisory locks, transaction-scoped: `SELECT pg_advisory_xact_lock(726354)` (`db.rs:727`, migrations),
  `SELECT pg_advisory_xact_lock(4815162342)` (`spaces.rs:89`, first-run setup),
  `SELECT pg_advisory_xact_lock(1618033988)` (`routes/members.rs:87`, owner-role changes).
- **Q7** `routes/health.rs:10` — `SELECT 1` (liveness probe, every container health check).

### 2.2 `workspace` (singleton)

- **Q8** point read of the one row, whole or projected: `SELECT * FROM workspace WHERE id = 1` (`spaces.rs:57`,
  called from `/me`, invitations, password mail, `me/update`, …); `SELECT name, logo …` (`routes/workspace.rs:49`,
  public); `SELECT logo …` (`routes/workspace.rs:113`); `SELECT settings …` (`pages.rs:427`).
- **Q9** `spaces.rs:97` — upsert of the singleton:
  ```sql
  INSERT INTO workspace (id, name) VALUES (1, $1)
  ON CONFLICT (id) DO UPDATE SET name = excluded.name, updated_at = now()
  ```
- **Q10** `spaces.rs:125` — `UPDATE workspace SET default_space_id = $1 WHERE id = 1`.
- **Q11** `routes/workspace.rs:78` — partial update: "absent keeps, null clears" via `COALESCE` and
  `CASE WHEN $given THEN $value ELSE col END`; array replace; **JSON shallow merge** `settings || $::jsonb`:
  ```sql
  UPDATE workspace SET
    name = COALESCE($1, name),
    description = CASE WHEN $2 THEN $3 ELSE description END,
    email_domains = COALESCE($4::text[], email_domains),
    settings = settings || $5::jsonb,
    updated_at = now()
  WHERE id = 1
  ```
- **Q12** `routes/workspace.rs:101` — **swap a value and return the previous one** (self-join trick):
  ```sql
  UPDATE workspace w SET logo = $1, updated_at = now() FROM workspace old
  WHERE w.id = 1 AND old.id = 1 RETURNING old.logo
  ```
- **Q13** `routes/workspace.rs:114` — `UPDATE workspace SET logo = NULL, updated_at = now() WHERE id = 1`.

### 2.3 `users`

Point lookups and existence checks

- **Q14** by primary key: `SELECT * FROM users WHERE id = $1` (`users.rs:167`);
  `SELECT password_hash FROM users WHERE id = $1` (`routes/auth.rs:180`, `routes/me.rs:114`);
  `SELECT email, name FROM users WHERE id = $1 AND deactivated_at IS NULL` (`routes/invitations.rs:246`).
- **Q15** `users.rs:175` — multi-get by id list (result re-ordered and de-duplicated by the app; called for
  every page/version/file/row response): `SELECT id, name, avatar FROM users WHERE id = ANY($1::uuid[])`.
- **Q16** by unique `auth_sub`: `SELECT * FROM users WHERE auth_sub = $1` (`sso.rs:195`, `sso.rs:227`);
  `SELECT id FROM users WHERE auth_sub = $1` (`sso.rs:528`).
- **Q17** by case-folded email (the expression-unique key) with a flag: `routes/auth.rs:85`
  `SELECT * FROM users WHERE lower(email) = $1 AND ($2 OR auth_sub IS NULL)`; `routes/auth.rs:110`
  `SELECT id, email, name FROM users WHERE lower(email) = $1 AND deactivated_at IS NULL AND ($2 OR auth_sub IS NULL)`
  (`$2` is a boolean computed by the app: "single sign-on is off").
- **Q18** email-taken checks: `SELECT 1 AS taken FROM users WHERE lower(email) = $1` (`sso.rs:232`,
  `routes/invitations.rs:215`); `… AND id <> $2` (`routes/me.rs:134`).
- **Q19** "any user at all?": `SELECT EXISTS (SELECT 1 FROM users) AS has_users` (`routes/auth.rs:38`, `:52`, `sso.rs:237`).
- **Q20** `routes/invitations.rs:134` — which of these emails already have accounts:
  `SELECT lower(email) AS email FROM users WHERE lower(email) = ANY($1::text[])`.

Lists, search, aggregates

- **Q21** `routes/members.rs:39` — filtered, ordered, offset-paginated list with a **window-function total**:
  ```sql
  SELECT u.*, count(*) OVER () AS total FROM users u
  WHERE $1 = '' OR lower(u.name) LIKE $2 OR lower(u.email) LIKE $2      -- $2 = '%' || escaped(q) || '%'
  ORDER BY u.deactivated_at IS NOT NULL, lower(u.name), u.id
  LIMIT $3 OFFSET $4
  ```
- **Q22** `routes/members.rs:54` — same predicate, `SELECT count(*) AS n FROM users u WHERE …` (only when the
  page is empty, to still report the total).
- **Q23** `routes/search.rs:43` — suggest users; substring match, **prefix matches ranked first** by ordering on a
  boolean expression:
  ```sql
  SELECT * FROM users u
  WHERE u.deactivated_at IS NULL AND (lower(u.name) LIKE $1 OR lower(u.email) LIKE $1)   -- '%q%'
  ORDER BY lower(u.name) LIKE $2 DESC, lower(u.name), u.id LIMIT $3                      -- $2 = 'q%'
  ```
- **Q24** owner counts: `SELECT count(*)::int AS owners FROM users WHERE role = 'owner' AND deactivated_at IS NULL`
  (`routes/members.rs:90`, inside the advisory lock); `… WHERE role = 'owner'` (`routes/members.rs:127`).

Writes

- **Q25** insert returning the whole row: `INSERT INTO users (email, name, password_hash, role) VALUES ($1,$2,$3,'owner') RETURNING *`
  (`routes/auth.rs:57`); `(email, name, password_hash, role, invited_by)` (`routes/invitations.rs:219`);
  `(email, name, role, auth_sub)` (`sso.rs:239`). All rely on `users_email_key`/`users_auth_sub_key` rejecting duplicates.
- **Q26** `auth.rs:98` — `UPDATE users SET last_login_at = now() WHERE id = $1`.
- **Q27** `UPDATE users SET password_hash = $1, updated_at = now() WHERE id = $2` (`routes/auth.rs:165`, `:184`).
- **Q28** role change: `UPDATE users SET role = $1, updated_at = now() WHERE id = $2 RETURNING *`
  (`routes/members.rs:98`); by provider subject, no return: `… WHERE auth_sub = $2` (`sso.rs:552`).
- **Q29** `routes/members.rs:110` — idempotent set-once:
  `UPDATE users SET deactivated_at = COALESCE(deactivated_at, now()), updated_at = now() WHERE id = $1`;
  `routes/members.rs:119` — `SET deactivated_at = NULL, updated_at = now()`.
- **Q30** `routes/me.rs:142` — profile patch; JSON merge then **strip null members** (null clears a preference):
  ```sql
  UPDATE users SET
    name = COALESCE($1, name), email = COALESCE($2, email),
    locale = CASE WHEN $3 THEN $4 ELSE locale END,
    timezone = CASE WHEN $5 THEN $6 ELSE timezone END,
    preferences = CASE WHEN $7 THEN jsonb_strip_nulls(preferences || $8::jsonb) ELSE preferences END,
    updated_at = now()
  WHERE id = $9
  ```
  (an email collision here is *not* caught: the pre-check Q18 can race, and the unique violation becomes a 500).
- **Q31** `routes/me.rs:170` — swap avatar, return the previous value:
  `UPDATE users u SET avatar = $1, updated_at = now() FROM users old WHERE u.id = $2 AND old.id = u.id RETURNING old.avatar`;
  `routes/me.rs:183` — `UPDATE users SET avatar = NULL, updated_at = now() WHERE id = $1`.
- **Q32** `sso.rs:203` — **conditional update whose RETURNING decides the flow** (link an account to a provider
  subject only if unlinked; 0 rows = someone else's or already linked; unique violation = raced):
  ```sql
  UPDATE users SET auth_sub = $1, updated_at = now()
  WHERE lower(email) = $2 AND auth_sub IS NULL RETURNING *
  ```
- **Q33** `sso.rs:341` — apply provider claims (retried without the email on a unique violation, §3.4):
  ```sql
  UPDATE users SET role = $1, name = $2, email = COALESCE($3, email), email_from_provider = $4, updated_at = now()
  WHERE id = $5 RETURNING *
  ```
- **Q34** `routes/members.rs:133` — `DELETE FROM users WHERE id = $1` (relies on the whole CASCADE / SET NULL
  fan-out of §1.28).

### 2.4 `sessions`

- **Q35** `auth.rs:153` — **on every API request**: session by token hash joined to its user, with expiry and
  activation checked against the database clock:
  ```sql
  SELECT s.id AS session_id, s.last_seen_at AS session_last_seen_at, s.auth_sid AS session_auth_sid,
         s.auth_refresh_token AS session_auth_refresh_token, s.auth_recheck_at AS session_auth_recheck_at, u.*
  FROM sessions s JOIN users u ON u.id = s.user_id
  WHERE s.token_hash = $1 AND s.expires_at > now() AND u.deactivated_at IS NULL
  ```
- **Q36** `auth.rs:191` — same join for a batch of session ids (the leader's once-a-minute socket check):
  `… WHERE s.id = ANY($1::uuid[]) AND s.expires_at > now() AND u.deactivated_at IS NULL`.
- **Q37** `auth.rs:84` — `INSERT INTO sessions (user_id, token_hash, user_agent, ip, expires_at, auth_sid, auth_refresh_token, auth_recheck_at) VALUES ($1..$8) RETURNING id`.
- **Q38** `auth.rs:167` — sliding expiry, at most once a minute per session:
  `UPDATE sessions SET last_seen_at = now(), expires_at = $1 WHERE id = $2`.
- **Q39** `sso.rs:460` — `UPDATE sessions SET auth_recheck_at = $1, auth_refresh_token = COALESCE($2, auth_refresh_token) WHERE id = $3`.
- **Q40** `routes/auth.rs:200` — a user's live sessions:
  `SELECT id, user_agent, ip, created_at, last_seen_at FROM sessions WHERE user_id = $1 AND expires_at > now() ORDER BY last_seen_at DESC`.
- **Q41** deletes by key: `DELETE FROM sessions WHERE id = $1` (`auth.rs:106`, `sso.rs:381`);
  `… WHERE id = $1 AND user_id = $2` (`routes/auth.rs:226`).
- **Q42** deletes by user: `DELETE FROM sessions WHERE user_id = $1` (`auth.rs:215`, `sso.rs:530`);
  all but one: `… WHERE user_id = $1 AND id <> $2` (`auth.rs:214`).
- **Q43** `sso.rs:545` — delete by the partial-indexed column, **returning the ids** to close sockets:
  `DELETE FROM sessions WHERE auth_sid = $1 RETURNING id`.
- **Q44** `auth.rs:221` — hourly purge: `DELETE FROM sessions WHERE expires_at < now()` (range delete, no index).

### 2.5 `user_tokens`

- **Q45** `routes/auth.rs:121` — expiry computed by the database:
  `INSERT INTO user_tokens (user_id, purpose, token_hash, expires_at) VALUES ($1, $2, $3, now() + make_interval(mins => $4))`.
- **Q46** `DELETE FROM user_tokens WHERE user_id = $1 AND purpose = $2` (`routes/auth.rs:120`, `:166`).
- **Q47** `routes/auth.rs:153` — **atomic single-use consume**: delete-with-join whose RETURNING is the proof the
  token was valid and unused (two concurrent resets: exactly one gets a row):
  ```sql
  DELETE FROM user_tokens t USING users u
  WHERE t.token_hash = $1 AND t.purpose = $2 AND t.expires_at > now()
    AND u.id = t.user_id AND u.deactivated_at IS NULL AND ($3 OR u.auth_sub IS NULL)
  RETURNING t.user_id
  ```
- **Q48** `auth.rs:222` — hourly purge: `DELETE FROM user_tokens WHERE expires_at < now()`.

### 2.6 `groups`, `group_members`

- **Q49** `routes/groups.rs:15` — group with a correlated member count; used with `WHERE g.id = $1` (`:49`) and
  as a full list `ORDER BY g.is_default DESC, lower(g.name)` (`:57`):
  ```sql
  SELECT g.id, g.name, g.description, g.is_default,
         (SELECT count(*)::int FROM group_members gm WHERE gm.group_id = g.id) AS member_count
  FROM groups g
  ```
- **Q50** insert: `INSERT INTO groups (name, description, created_by) VALUES ($1,$2,$3) RETURNING id`
  (`routes/groups.rs:67`, unique violation → 409); the seeded default group
  `INSERT INTO groups (name, description, is_default, created_by) VALUES ('Everyone', 'Every member of the workspace', true, $1) RETURNING id` (`spaces.rs:104`).
- **Q51** `routes/groups.rs:90` — `UPDATE groups SET name = COALESCE($1, name), description = CASE WHEN $2 THEN $3 ELSE description END, updated_at = now() WHERE id = $4` (unique violation → 409).
- **Q52** `routes/groups.rs:117` — `DELETE FROM groups WHERE id = $1` (cascades to `group_members` and
  `space_members`, which silently changes who can read which space).
- **Q53** `routes/invitations.rs:128` — `SELECT id FROM groups WHERE id = ANY($1::uuid[]) AND NOT is_default`.
- **Q54** member ids of groups: `SELECT user_id FROM group_members WHERE group_id = $1` (`routes/groups.rs:53`);
  `… WHERE group_id = ANY($1::uuid[])` (`routes/spaces.rs:285`).
- **Q55** `routes/groups.rs:128` — members page (join, case-folded order, offset pagination; the total comes
  from Q49's count):
  `SELECT u.* FROM group_members gm JOIN users u ON u.id = gm.user_id WHERE gm.group_id = $1 ORDER BY lower(u.name), u.id LIMIT $2 OFFSET $3`.
- **Q56** plain insert `INSERT INTO group_members (group_id, user_id) VALUES ($1, $2)` (`spaces.rs:112`).
- **Q57** **insert-from-select, ignoring existing rows** (the select filters the ids down to ones that exist, so
  no FK error can occur):
  `INSERT INTO group_members (group_id, user_id) SELECT $1, id FROM users WHERE id = ANY($2::uuid[]) ON CONFLICT DO NOTHING` (`routes/groups.rs:146`);
  join the default group(s): `… SELECT id, $1 FROM groups WHERE is_default ON CONFLICT DO NOTHING` (`sso.rs:252`);
  default groups plus invited ones: `… SELECT id, $1 FROM groups WHERE is_default OR id = ANY($2::uuid[]) ON CONFLICT DO NOTHING` (`routes/invitations.rs:232`).
- **Q58** `routes/groups.rs:165` — `DELETE FROM group_members WHERE group_id = $1 AND user_id = $2`.

### 2.7 `invitations`

- **Q59** `routes/invitations.rs:55` — by id with a TTL computed in SQL; the array column is read as `text[]`:
  `SELECT id, email, role, group_ids::text[] AS group_ids, token_hash, invited_by FROM invitations WHERE id = $1 AND created_at > now() - make_interval(days => $2)`.
- **Q60** `routes/invitations.rs:80` — full list with a LEFT JOIN to the inviter:
  `SELECT i.id, i.email, i.role, i.group_ids::text[] AS group_ids, i.created_at, u.id AS inviter_id, u.name AS inviter_name, u.avatar AS inviter_avatar FROM invitations i LEFT JOIN users u ON u.id = i.invited_by ORDER BY i.created_at DESC`.
- **Q61** `routes/invitations.rs:144` — **upsert on an expression unique index**, replacing every field:
  ```sql
  INSERT INTO invitations (email, role, group_ids, token_hash, invited_by) VALUES ($1, $2, $3::uuid[], $4, $5)
  ON CONFLICT (lower(email)) DO UPDATE SET
    role = excluded.role, group_ids = excluded.group_ids, token_hash = excluded.token_hash,
    invited_by = excluded.invited_by, created_at = now()
  RETURNING id
  ```
- **Q62** `routes/invitations.rs:71` — rotate; 0 rows returned = 404:
  `UPDATE invitations SET token_hash = $1, created_at = now() WHERE id = $2 RETURNING id, email`.
- **Q63** `DELETE FROM invitations WHERE id = $1` (`routes/invitations.rs:186`); and the **claim-once** form whose
  RETURNING decides whether the acceptance proceeds: `DELETE FROM invitations WHERE id = $1 RETURNING id` (`:211`).

### 2.8 `spaces`, `space_members`

- **Q64** `spaces.rs:15` — the "space with my role and member count" base query (two correlated scalar subqueries):
  ```sql
  SELECT s.id, s.name, s.slug, s.description, s.icon, s.settings, s.created_at,
         «F3 role_rank("s.id", $u)» AS rank,
         (SELECT count(*)::int FROM space_members sm WHERE sm.space_id = s.id) AS member_count
  FROM spaces s
  ```
  used as: by id `… WHERE s.id = $1` or by case-folded slug `… WHERE lower(s.slug) = $1` (`spaces.rs:51-52`);
  all spaces `… ORDER BY lower(s.name), s.id` (`routes/spaces.rs:67`, admins);
  **as a derived table filtered on the computed column**
  `SELECT * FROM (Q64) s WHERE s.rank IS NOT NULL ORDER BY lower(s.name), s.id` (`routes/spaces.rs:61`);
  joined to favourites `SELECT s.* FROM (Q64) s JOIN favorites f ON f.space_id = s.id WHERE f.user_id = $1 AND s.rank IS NOT NULL ORDER BY f.created_at` (`routes/favorites.rs:28`).
- **Q65** role of one user in one space: `SELECT «F3» AS rank FROM spaces s WHERE s.id = $1` (`permissions.rs:75`;
  also distinguishes "space missing" from "no role" in `SELECT id, «F3» AS rank FROM spaces s WHERE s.id = $1`, `:92`).
- **Q66** `permissions.rs:105` — every space a user can read (run at each WebSocket connect and after each
  membership change): `SELECT DISTINCT space_id FROM «F4» AS r`.
- **Q67** `routes/shares.rs:155` — evaluate a predicate for one space: `SELECT «F7 sharing_enabled(s)» AS ok FROM spaces s WHERE s.id = $1`.
- **Q68** inserts: `INSERT INTO spaces (name, slug, description, created_by) VALUES ($1,$2,$3,$4) RETURNING id`
  (`routes/spaces.rs:79`, slug unique violation → 409); `INSERT INTO spaces (name, slug, created_by) VALUES ('General','general',$1) RETURNING id` (`spaces.rs:114`).
- **Q69** `routes/spaces.rs:129` — patch with JSON merge (slug unique violation → 409):
  ```sql
  UPDATE spaces SET name = COALESCE($1, name), slug = COALESCE($2, slug),
    description = CASE WHEN $3 THEN $4 ELSE description END,
    settings = settings || $5::jsonb, updated_at = now()
  WHERE id = $6
  ```
- **Q70** icon: `UPDATE spaces SET icon = $1, updated_at = now() WHERE id = $2` (`routes/spaces.rs:189`);
  `… SET icon = NULL …` (`:199`).
- **Q71** `routes/spaces.rs:165` — **delete a space and return the storage keys of every file it owned, in one
  statement** (two data-modifying CTEs; the space delete then cascades to pages and everything under them):
  ```sql
  WITH removed_files AS (
    DELETE FROM files WHERE space_id = $1 OR page_id IN (SELECT id FROM pages WHERE space_id = $1)
    RETURNING storage_key),
  removed_space AS (DELETE FROM spaces WHERE id = $1)
  SELECT storage_key FROM removed_files
  ```
- **Q72** `routes/spaces.rs:162` — `SELECT id FROM space_members WHERE space_id = $1`.
- **Q73** `routes/spaces.rs:44` — users affected by membership rows, directly or through a group (**UNION**, dedup):
  ```sql
  SELECT sm.user_id FROM space_members sm WHERE sm.id = ANY($1::uuid[]) AND sm.user_id IS NOT NULL
  UNION
  SELECT gm.user_id FROM space_members sm JOIN group_members gm ON gm.group_id = sm.group_id
  WHERE sm.id = ANY($1::uuid[])
  ```
- **Q74** `routes/spaces.rs:211` — members list: two LEFT JOINs, nested JSON objects built per row, a correlated
  count inside the JSON, and a three-key order (groups first, then case-folded name):
  ```sql
  SELECT sm.id, sm.role,
    CASE WHEN u.id IS NULL THEN NULL ELSE json_build_object('id', u.id, 'email', u.email, 'name', u.name,
      'avatar', u.avatar, 'role', u.role, 'locale', u.locale, 'timezone', u.timezone, 'preferences', '{}'::json,
      'created_at', u.created_at, 'last_login_at', u.last_login_at, 'deactivated_at', u.deactivated_at) END AS "user",
    CASE WHEN g.id IS NULL THEN NULL ELSE json_build_object('id', g.id, 'name', g.name, 'description', g.description,
      'is_default', g.is_default,
      'member_count', (SELECT count(*) FROM group_members gm WHERE gm.group_id = g.id)) END AS "group"
  FROM space_members sm LEFT JOIN users u ON u.id = sm.user_id LEFT JOIN groups g ON g.id = sm.group_id
  WHERE sm.space_id = $1
  ORDER BY sm.group_id IS NULL, lower(coalesce(g.name, u.name)), sm.id
  ```
  (timestamps inside the JSON arrive as Postgres' JSON text form `2026-01-02T03:04:05.123456+00:00` and are
  re-parsed by the app, `users.rs:87`).
- **Q75** plain inserts of the creator as admin: `INSERT INTO space_members (space_id, user_id, role, added_by) VALUES ($1, $2, 'admin', $3)` (`routes/spaces.rs:89`, `spaces.rs:118`).
- **Q76** `routes/spaces.rs:259`, `:271` — **bulk upsert on a partial unique index** (conflict target includes the
  index predicate; existing members get the new role):
  ```sql
  INSERT INTO space_members (space_id, user_id, role, added_by)
  SELECT $1::uuid, id, $2::text, $3::uuid FROM users WHERE id = ANY($4::uuid[])
  ON CONFLICT (space_id, user_id) WHERE user_id IS NOT NULL DO UPDATE SET role = excluded.role
  -- and the same with group_id / FROM groups / ON CONFLICT (space_id, group_id) WHERE group_id IS NOT NULL
  ```
- **Q77** row lock on one membership: `SELECT id, role FROM space_members WHERE id = $1 AND space_id = $2 FOR UPDATE`
  (`routes/spaces.rs:300`, `:326`).
- **Q78** `SELECT count(*)::int AS admins FROM space_members WHERE space_id = $1 AND role = 'admin'`
  (`routes/spaces.rs:304`, `:330`) — the "last admin" guard, inside the transaction of Q77.
- **Q79** `UPDATE space_members SET role = $1 WHERE id = $2` (`routes/spaces.rs:311`);
  `DELETE FROM space_members WHERE id = $1` (`:337`).

### 2.9 `pages` — point lookups and access checks

- **Q80** `pages.rs:114` — **the access check behind almost every page endpoint** (`find_page`/`require_page`,
  and twice per WebSocket `join`): a page by id *or* by slug, optionally with its document, plus the caller's role:
  ```sql
  SELECT «F2 page_columns», p.content  /* or NULL::jsonb AS content */, «F3 role_rank("p.space_id", $u)» AS rank
  FROM pages p WHERE p.id = $1          /* or: p.slug_id = $1 */
  ```
- **Q81** `routes/pages.rs:37` — `SELECT «F2», p.content FROM pages p WHERE p.id = $1` (re-read after every write
  to build the response); `routes/bases.rs:488` — `SELECT «F1» FROM pages p WHERE p.id = $1`.
- **Q82** `pages.rs:165` — load for collaboration: `SELECT content, doc_version, space_id, deleted_at FROM pages WHERE id = $1`.
- **Q83** `pages.rs:171` — `SELECT doc_version FROM pages WHERE id = $1` (staleness probe of a kept session).
- **Q84** parent validation inside create/move transactions:
  `SELECT id FROM pages WHERE id = $1 AND space_id = $2 AND deleted_at IS NULL` (`routes/pages.rs:201`, `:298`, `routes/bases.rs:453`).
- **Q85** `routes/shares.rs:89` — public page (no user): `SELECT «F1», p.cover, p.content, p.created_at, p.updated_at FROM pages p WHERE (p.id = $1 | p.slug_id = $1) AND p.deleted_at IS NULL`.
- **Q86** export loads: `SELECT id, slug_id, parent_id, title, is_base FROM pages WHERE id = $1` (`export.rs:393`);
  contents in batches of 50: `SELECT id, content FROM pages WHERE id = ANY($1::uuid[])` (`export.rs:332`).

### 2.10 `pages` — lists and ordering

- **Q87** `routes/pages.rs:122` — children of a node in sibling order (the sidebar tree; **null-safe equality**
  so the same query serves roots):
  ```sql
  SELECT «F1» FROM pages p
  WHERE p.space_id = $1 AND p.parent_id IS NOT DISTINCT FROM $2::uuid AND p.deleted_at IS NULL
  ORDER BY p.position, p.id        -- position is COLLATE "C"
  ```
- **Q88** `routes/pages.rs:141` — recent pages across readable spaces: inner join + left join, optional filters
  written as `($x IS NULL OR col = $x)`, offset pagination:
  ```sql
  SELECT «F1», p.updated_at, «F5 space_ref_json(s)» AS space,
         u.id AS editor_id, u.name AS editor_name, u.avatar AS editor_avatar
  FROM pages p JOIN spaces s ON s.id = p.space_id LEFT JOIN users u ON u.id = p.last_editor_id
  WHERE p.deleted_at IS NULL AND p.space_id IN «F4»
    AND ($1::uuid IS NULL OR p.space_id = $1::uuid) AND ($2::uuid IS NULL OR p.creator_id = $2::uuid)
  ORDER BY p.updated_at DESC, p.id LIMIT $3 OFFSET $4
  ```
- **Q89** `routes/pages.rs:403` — trash list: self LEFT JOIN to the parent to show only the *roots* of each
  trashed subtree (a child is hidden when its parent was trashed at the very same instant):
  ```sql
  SELECT «F1», p.deleted_at, u.id AS deleter_id, u.name AS deleter_name, u.avatar AS deleter_avatar
  FROM pages p LEFT JOIN pages parent ON parent.id = p.parent_id LEFT JOIN users u ON u.id = p.deleted_by
  WHERE p.space_id = $1 AND p.deleted_at IS NOT NULL
    AND (parent.id IS NULL OR parent.deleted_at IS NULL OR parent.deleted_at <> p.deleted_at)
  ORDER BY p.deleted_at DESC, p.id LIMIT 1000
  ```
- **Q90** `export.rs:395` — whole space: `SELECT id, slug_id, parent_id, title, is_base FROM pages WHERE space_id = $1 AND deleted_at IS NULL ORDER BY position, id`.
- **Q91** `routes/search.rs:57` — suggest pages by title substring, prefix matches first:
  ```sql
  SELECT «F1», «F5» AS space FROM pages p JOIN spaces s ON s.id = p.space_id
  WHERE p.deleted_at IS NULL AND p.space_id IN «F4» AND ($1::uuid IS NULL OR p.space_id = $1::uuid)
    AND lower(coalesce(p.title, '')) LIKE $2                               -- '%q%'
  ORDER BY lower(coalesce(p.title, '')) LIKE $3 DESC, p.updated_at DESC, p.id LIMIT $4   -- 'q%'
  ```
- **Q92** `bases.rs:794` — resolve page ids referenced by cells, restricted to what the user can read:
  `SELECT p.id, p.slug_id, p.title, p.icon, s.slug AS space_slug FROM pages p JOIN spaces s ON s.id = p.space_id WHERE p.id = ANY($1::uuid[]) AND p.deleted_at IS NULL AND p.space_id IN «F4»`.
- **Q93** `export.rs:295` — the same by slug ids: `SELECT p.slug_id, p.title, s.slug AS space_slug FROM pages p JOIN spaces s ON s.id = p.space_id WHERE p.slug_id = ANY($1::text[]) AND p.deleted_at IS NULL AND p.space_id IN (…F4 inlined…)`.
- **Q94** `notifications.rs:91` — mail context with a scalar subquery: `SELECT p.title, p.slug_id, s.slug AS space_slug, (SELECT name FROM users WHERE id = $1) AS actor_name FROM pages p JOIN spaces s ON s.id = p.space_id WHERE p.id = $2`.

### 2.11 `pages` — sibling-position maintenance (fractional indexing)

The app keeps order with string keys and computes a new key *between two neighbours* (`common/positions.rs`,
`key_between(low, high)`); the database only has to find the neighbours, with bytewise string comparison.
All four run inside the transaction that then writes the page. Sibling predicate **S** (`pages.rs:205`):
`p.space_id = $1 AND p.parent_id IS NOT DISTINCT FROM $2::uuid AND p.deleted_at IS NULL AND p.id IS DISTINCT FROM $3::uuid`.

- **Q95** append: `SELECT max(p.position) AS position FROM pages p WHERE «S»` (`pages.rs:217`).
- **Q96** prepend: `SELECT min(p.position) AS position FROM pages p WHERE «S»` (`pages.rs:221`).
- **Q97** after a given sibling: `SELECT p.position FROM pages p WHERE p.id = $4 AND «S»` (`pages.rs:225`, no row → 404),
  then the next key: `SELECT min(p.position) AS position FROM pages p WHERE «S» AND p.position > $5` (`pages.rs:228`).

There is **no renumbering and no uniqueness on position**: two concurrent inserts between the same neighbours get
the same key and are tie-broken by `id` in every ORDER BY.

### 2.12 `pages` — recursive queries over the tree

Ancestors (walk `parent_id` upward; `UNION ALL` with a depth guard of 1000):

- **Q98** `pages.rs:178` — breadcrumbs, root first:
  ```sql
  WITH RECURSIVE chain AS (
    SELECT id, parent_id, 0 AS depth FROM pages WHERE id = $1
    UNION ALL
    SELECT p.id, p.parent_id, chain.depth + 1 FROM pages p JOIN chain ON p.id = chain.parent_id WHERE chain.depth < 1000)
  SELECT «F1» FROM chain JOIN pages p ON p.id = chain.id ORDER BY chain.depth DESC
  ```
- **Q99** `pages.rs:488` — cycle check for a move ("is the new parent inside the moved subtree?"): same `chain`
  from the candidate parent, then `SELECT EXISTS (SELECT 1 FROM chain WHERE id = $2) AS inside`.
- **Q100** `shares.rs:103` — "does this public share expose that page?": ancestor chain starting at a non-trashed
  page, **stopping at the shared root** (`… WHERE chain.depth < 1000 AND chain.id <> $2`), then
  `SELECT EXISTS (SELECT 1 FROM chain WHERE id = $2) AS allowed`. Runs for every public page view and every file
  download through a share.
- **Q101** `routes/shares.rs:234` — public breadcrumbs: the same bounded chain (stop at the shared root), joined
  back to pages, `ORDER BY chain.depth DESC`.
- **Q102** `routes/shares.rs:124` — nearest ancestor that carries a share with `include_children` (recursive CTE
  joined to two tables, ordered by depth, LIMIT 1):
  ```sql
  WITH RECURSIVE chain AS (…Q98's chain from $1…)
  SELECT sh.id AS share_id, sh.key, sh.include_children, sh.search_indexing, sh.created_at AS share_created_at, «F1»
  FROM chain JOIN shares sh ON sh.page_id = chain.id JOIN pages p ON p.id = chain.id
  WHERE chain.depth > 0 AND sh.include_children ORDER BY chain.depth LIMIT 1
  ```

Descendants (walk children downward):

- **Q103** `routes/pages.rs:48` — ids of a subtree, trashed or not (`UNION ALL`, depth guard):
  ```sql
  WITH RECURSIVE subtree AS (
    SELECT id, 0 AS depth FROM pages WHERE id = $1
    UNION ALL
    SELECT p.id, s.depth + 1 FROM pages p JOIN subtree s ON p.parent_id = s.id WHERE s.depth < 1000)
  SELECT id FROM subtree
  ```
- **Q104** live subtree with columns, **ordered breadth-first by depth then sibling position**, capped:
  `routes/shares.rs:101` — `… WHERE p.deleted_at IS NULL AND s.depth < 1000) SELECT «F1» FROM subtree s JOIN pages p ON p.id = s.id WHERE s.depth > 0 ORDER BY s.depth, p.parent_id, p.position, p.id LIMIT 5000`;
  `duplicate.rs:40` — `… SELECT p.id, p.parent_id, p.position, p.title, p.icon, p.cover, p.content, p.is_base FROM subtree s JOIN pages p ON p.id = s.id ORDER BY s.depth, p.position LIMIT 2001` (whole documents of up to 2000 pages in one result);
  `export.rs:380` — `… SELECT p.id, p.slug_id, p.parent_id, p.title, p.is_base FROM subtree s JOIN pages p ON p.id = s.id ORDER BY s.depth, p.position, p.id`.
- **Q105** `routes/shares.rs:257` — a subtree as a **search scope** (recursive CTE inside `IN (…)`, spliced into
  the full-text query Q118): `p.id IN (WITH RECURSIVE subtree AS (SELECT id FROM pages WHERE id = $1 UNION SELECT c.id FROM pages c JOIN subtree t ON c.parent_id = t.id WHERE c.deleted_at IS NULL) SELECT id FROM subtree)`.

Recursive **writes** (each is one statement = one atomic change; `UNION` rather than `UNION ALL`, no depth guard):

- **Q106** `pages.rs:435` — move a subtree to the trash and drop its public shares; every row gets the *same*
  `deleted_at` (that equality is what restore and the trash list key on):
  ```sql
  WITH RECURSIVE subtree AS (
    SELECT id FROM pages WHERE id = $1 AND deleted_at IS NULL
    UNION
    SELECT p.id FROM pages p JOIN subtree s ON p.parent_id = s.id WHERE p.deleted_at IS NULL),
  removed_shares AS (DELETE FROM shares WHERE page_id IN (SELECT id FROM subtree))
  UPDATE pages SET deleted_at = now(), deleted_by = $2 WHERE id IN (SELECT id FROM subtree)
  ```
- **Q107** `pages.rs:468` — restore the page and exactly the descendants trashed in the same operation (recursion
  joins on **timestamp equality** with the root):
  ```sql
  WITH RECURSIVE root AS (SELECT deleted_at FROM pages WHERE id = $1),
  subtree AS (
    SELECT id FROM pages WHERE id = $1
    UNION
    SELECT p.id FROM pages p JOIN subtree s ON p.parent_id = s.id JOIN root ON p.deleted_at = root.deleted_at)
  UPDATE pages SET deleted_at = NULL, deleted_by = NULL WHERE id IN (SELECT id FROM subtree)
  ```
- **Q108** `pages.rs:402` — permanent delete of one or many subtrees, **returning both the storage keys of the
  removed files and the ids of the removed pages** as one result (`UNION ALL` of two `RETURNING` sets). The root
  condition is spliced: `id = $1` (`pages.rs:422`) or, for the hourly purge,
  `deleted_at IS NOT NULL AND deleted_at < now() - make_interval(days => $1::int)` (`pages.rs:430`):
  ```sql
  WITH RECURSIVE subtree AS (
    SELECT id FROM pages WHERE «root condition»
    UNION
    SELECT p.id FROM pages p JOIN subtree s ON p.parent_id = s.id),
  removed_files AS (DELETE FROM files WHERE page_id IN (SELECT id FROM subtree) RETURNING storage_key),
  removed_pages AS (DELETE FROM pages WHERE id IN (SELECT id FROM subtree) RETURNING id)
  SELECT storage_key, NULL::uuid AS page_id FROM removed_files
  UNION ALL
  SELECT NULL, id FROM removed_pages
  ```
- **Q109** `routes/pages.rs:328` — move a subtree to another space: re-home its files, conditionally drop its
  shares (if the target space has sharing disabled), and rewrite three columns with per-row `CASE` (only the
  root loses its parent and gets a new position):
  ```sql
  WITH RECURSIVE subtree AS (SELECT id FROM pages WHERE id = $1 UNION SELECT p.id FROM pages p JOIN subtree s ON p.parent_id = s.id),
  moved_files AS (UPDATE files SET space_id = $2 WHERE page_id IN (SELECT id FROM subtree)),
  dropped_shares AS (
    DELETE FROM shares WHERE page_id IN (SELECT id FROM subtree)
      AND (SELECT coalesce((settings->>'sharingDisabled')::boolean, false) FROM spaces WHERE id = $2))
  UPDATE pages SET space_id = $2,
    parent_id = CASE WHEN id = $1 THEN NULL ELSE parent_id END,
    position  = CASE WHEN id = $1 THEN $3 ELSE position END
  WHERE id IN (SELECT id FROM subtree)
  ```

### 2.13 `pages` — writes

- **Q110** create: `routes/pages.rs:209` —
  `INSERT INTO pages (slug_id, space_id, parent_id, position, title, icon, content, text_content, creator_id, last_editor_id, contributor_ids) VALUES ($1,$2,$3,$4,$5,$6,$7::jsonb,$8,$9,$10, ARRAY[$11]::uuid[]) RETURNING id`;
  `routes/bases.rs:462` — the same plus `is_base = true`. (A `slug_id` collision is not handled: it would be a 500.)
- **Q111** `routes/pages.rs:247` — metadata patch:
  ```sql
  UPDATE pages SET
    title = CASE WHEN $1 THEN $2 ELSE title END, icon = CASE WHEN $3 THEN $4 ELSE icon END,
    cover = CASE WHEN $5 THEN $6 ELSE cover END, last_editor_id = $7, updated_at = now()
  WHERE id = $8
  ```
- **Q112** move within a space: `UPDATE pages SET parent_id = $1, position = $2 WHERE id = $3` (`routes/pages.rs:311`);
  restore to the root: `UPDATE pages SET parent_id = NULL, position = $1 WHERE id = $2` (`pages.rs:466`).
- **Q113** `routes/bases.rs:523` — `UPDATE pages SET is_base = true, updated_at = now(), last_editor_id = $1 WHERE id = $2`.
- **Q114** `pages.rs:304` — **lock the row and read the version** (first statement of the save transaction):
  `SELECT content, space_id, deleted_at, doc_version FROM pages WHERE id = $1 FOR UPDATE`.
- **Q115** `pages.rs:309` — the save itself: replace the document, its text, the version; keep-or-set the last
  editor; **array set-union**:
  ```sql
  UPDATE pages SET
    content = $1::jsonb, text_content = $2, doc_version = $3,
    last_editor_id = COALESCE($4::uuid, last_editor_id),
    contributor_ids = ARRAY(SELECT DISTINCT unnest(contributor_ids || $5::uuid[])),
    updated_at = now()
  WHERE id = $6
  ```
  (also recomputes the stored `search` tsvector; the resulting order of `contributor_ids` is unspecified).
- **Q116** `pages.rs:455` — lock for restore while reading the parent through a LEFT JOIN (lock only the page row):
  `SELECT p.id, p.space_id, p.parent_id, p.deleted_at, parent.deleted_at AS parent_deleted_at, parent.id AS existing_parent FROM pages p LEFT JOIN pages parent ON parent.id = p.parent_id WHERE p.id = $1 FOR UPDATE OF p`.
- **Q117** `duplicate.rs:155` — **bulk insert of up to 2000 pages from one JSON parameter**, with app-chosen ids so
  parent links inside the batch already point at the new rows:
  ```sql
  INSERT INTO pages (id, slug_id, space_id, parent_id, position, title, icon, cover, content, text_content,
                     creator_id, last_editor_id, contributor_ids, is_base)
  SELECT r.id, r.slug_id, $1::uuid, r.parent_id, r.position, r.title, r.icon, r.cover, r.content, r.text_content,
         $2::uuid, $2::uuid, ARRAY[$2]::uuid[], r.is_base
  FROM jsonb_to_recordset($3::jsonb)
    AS r(id uuid, slug_id text, parent_id uuid, position text, title text, icon text, cover text, content jsonb,
         text_content text, is_base boolean)
  ```
  (rows are sent parents-first, ordered by depth; in PostgreSQL the self-referencing FK is in any case checked
  at the end of the statement, so one statement can insert a whole subtree.)

### 2.14 `pages` — full-text search (details in §4.2)

- **Q118** `search.rs:172` — main search (prefix full-text OR title substring; exact-title first, then rank, then recency):
  ```sql
  SELECT «F1», p.updated_at, «F5» AS space, «snippet» AS snippet
  FROM pages p JOIN spaces s ON s.id = p.space_id
  LEFT JOIN LATERAL (SELECT CASE WHEN $1::text IS NULL THEN NULL ELSE to_tsquery('simple', $1) END AS q) query ON true
  WHERE p.deleted_at IS NULL AND «scope»
    AND ((query.q IS NOT NULL AND «vector» @@ query.q) OR lower(coalesce(p.title, '')) LIKE $2)
  ORDER BY lower(coalesce(p.title, '')) = $3 DESC, coalesce(ts_rank(«vector», query.q), 0) DESC, p.updated_at DESC, p.id
  LIMIT $4 OFFSET $5
  ```
  `«vector»` = `p.search` or, for title-only, `to_tsvector('simple', coalesce(p.title, ''))` (`search.rs:165`);
  `«snippet»` = `p.title` or `substring(p.text_content FROM greatest(1, strpos(lower(p.text_content), $w) - 100) FOR 400)` (`search.rs:167-169`);
  `«scope»` = `p.space_id IN «F4» AND ($s::uuid IS NULL OR p.space_id = $s::uuid)` (`routes/search.rs:19`), or for
  public shares `p.id = $1` / Q105.
- **Q119** `search.rs:197` — fallback when Q118 found nothing (not title-only, first page only): raw
  case-insensitive substring scan of the document text:
  ```sql
  SELECT «F1», p.updated_at, «F5» AS space,
         substring(p.text_content FROM greatest(1, strpos(lower(p.text_content), $1) - 100) FOR 400) AS snippet
  FROM pages p JOIN spaces s ON s.id = p.space_id
  WHERE p.deleted_at IS NULL AND «scope» AND p.text_content ILIKE $2      -- '%escaped(q)%'
  ORDER BY p.updated_at DESC, p.id LIMIT $3                               -- min(limit, 20)
  ```

### 2.15 `page_versions`

- **Q120** `pages.rs:327` — **conditional snapshot** at the end of each save: copy the page into history unless a
  version newer than 5 minutes exists (60 seconds while the page itself is younger than 5 minutes), and unless
  the content equals the latest version's content (jsonb structural comparison, null-safe):
  ```sql
  INSERT INTO page_versions (page_id, title, icon, content, editor_id, contributor_ids)
  SELECT p.id, p.title, p.icon, p.content, p.last_editor_id, p.contributor_ids FROM pages p
  WHERE p.id = $1
    AND NOT EXISTS (
      SELECT 1 FROM page_versions v WHERE v.page_id = p.id
        AND v.created_at > now() - CASE WHEN p.created_at > now() - interval '5 minutes'
                                        THEN interval '60 seconds' ELSE interval '5 minutes' END)
    AND p.content IS DISTINCT FROM (
      SELECT v.content FROM page_versions v WHERE v.page_id = p.id ORDER BY v.created_at DESC LIMIT 1)
  ```
- **Q121** `routes/pages.rs:445` — newest 500 (without content): `SELECT v.id, v.title, v.created_at, v.editor_id FROM page_versions v WHERE v.page_id = $1 ORDER BY v.created_at DESC LIMIT 500`.
- **Q122** `routes/pages.rs:472` — one version with content: `SELECT v.id, v.page_id, v.title, v.icon, v.content, v.editor_id, v.contributor_ids::text[] AS contributor_ids, v.created_at FROM page_versions v WHERE v.id = $1`.

Versions are never pruned (only removed by cascade).

### 2.16 `page_links`, `synced_blocks` (derived indexes, rebuilt in the save transaction — `index_page_content`, `pages.rs:237`)

- **Q123** `pages.rs:239` — `DELETE FROM page_links WHERE source_id = $1`.
- **Q124** `pages.rs:242` — re-insert links by resolving two id lists against existing pages:
  ```sql
  INSERT INTO page_links (source_id, target_id)
  SELECT $1::uuid, id FROM pages
  WHERE (id = ANY($2::uuid[]) OR slug_id = ANY($3::text[])) AND id <> $1
  ON CONFLICT DO NOTHING
  ```
- **Q125** `duplicate.rs:245` — the same for many source pages at once (join against arrays inside a JSON recordset):
  ```sql
  INSERT INTO page_links (source_id, target_id)
  SELECT r.id, p.id FROM jsonb_to_recordset($1::jsonb) AS r(id uuid, page_ids text[], slug_ids text[])
  JOIN pages p ON (p.id::text = ANY(r.page_ids) OR p.slug_id = ANY(r.slug_ids)) AND p.id <> r.id
  ON CONFLICT DO NOTHING
  ```
- **Q126** `routes/pages.rs:529` — backlinks / outgoing links (the join column and the filter column swap):
  ```sql
  SELECT «F1», «F5» AS space
  FROM page_links l JOIN pages p ON p.id = l.source_id /* or l.target_id */ JOIN spaces s ON s.id = p.space_id
  WHERE l.target_id /* or l.source_id */ = $1 AND p.deleted_at IS NULL AND p.space_id IN «F4»
  ORDER BY lower(coalesce(p.title, '')), p.id LIMIT 500
  ```
- **Q127** `pages.rs:257` — delete all blocks **not in** a list: `DELETE FROM synced_blocks WHERE page_id = $1 AND NOT (block_id = ANY($2::text[]))`.
- **Q128** `pages.rs:260` — bulk upsert on the composite key, **updating only rows whose content actually changed**
  (so `updated_at` moves only then):
  ```sql
  INSERT INTO synced_blocks (page_id, block_id, content)
  SELECT $1::uuid, r.block_id, r.content FROM jsonb_to_recordset($2::jsonb) AS r(block_id text, content jsonb)
  ON CONFLICT (page_id, block_id) DO UPDATE SET content = excluded.content, updated_at = now()
  WHERE synced_blocks.content IS DISTINCT FROM excluded.content
  ```
- **Q129** `duplicate.rs:261` — bulk insert for many pages: `INSERT INTO synced_blocks (page_id, block_id, content) SELECT r.page_id, r.block_id, r.content FROM jsonb_to_recordset($1::jsonb) AS r(page_id uuid, block_id text, content jsonb) ON CONFLICT DO NOTHING`.
- **Q130** `routes/pages.rs:570` — batch lookup of up to 100 `(page, block)` references **preserving request order**,
  with the permission check folded into the join (a reference the user cannot read comes back with NULL content):
  ```sql
  SELECT r.page_id, r.block_id, sb.content
  FROM jsonb_to_recordset($1::jsonb) AS r(page_id uuid, block_id text, ord int)
  LEFT JOIN pages p ON p.id = r.page_id AND p.deleted_at IS NULL AND p.space_id IN «F4»
  LEFT JOIN synced_blocks sb ON sb.page_id = p.id AND sb.block_id = r.block_id
  ORDER BY r.ord
  ```

### 2.17 `comments`

- **Q131** `routes/comments.rs:24` — comment with author and resolver as JSON refs (two LEFT JOINs to `users`):
  ```sql
  SELECT c.id, c.page_id, c.parent_id, c.author_id, c.body, c.quote, c.anchored, c.created_at, c.edited_at, c.resolved_at,
         «F6 user_ref_json(a)» AS author, «F6 user_ref_json(r)» AS resolver
  FROM comments c LEFT JOIN users a ON a.id = c.author_id LEFT JOIN users r ON r.id = c.resolved_by
  ```
  by id `… WHERE c.id = $1` (`:53`); thread of a page `… WHERE c.page_id = $1 ORDER BY c.created_at, c.id` (`:116`, unpaginated).
- **Q132** `routes/comments.rs:129` — `SELECT id, parent_id FROM comments WHERE id = $1 AND page_id = $2`.
- **Q133** `routes/comments.rs:88` — everyone in a thread: `SELECT DISTINCT author_id AS id FROM comments WHERE (id = $1 OR parent_id = $1) AND author_id IS NOT NULL`.
- **Q134** `routes/comments.rs:134` — `INSERT INTO comments (page_id, parent_id, author_id, body, quote, anchored) VALUES ($1,$2,$3,$4::jsonb,$5,$6) RETURNING id`.
- **Q135** `routes/comments.rs:167` — `UPDATE comments SET body = $1::jsonb, edited_at = now(), updated_at = now() WHERE id = $2`.
- **Q136** `routes/comments.rs:180` — resolve keeps the first resolution time, unresolve clears it (the SET
  expression itself is chosen by the app): `UPDATE comments SET resolved_at = COALESCE(resolved_at, now()) /* or NULL */, resolved_by = $1, updated_at = now() WHERE id = $2`.
- **Q137** `routes/comments.rs:201` — `DELETE FROM comments WHERE id = $1` (cascades to replies and to notifications about them).

### 2.18 `files` (metadata; the bytes are in object storage)

- **Q138** page-file inserts with an app-chosen id: `INSERT INTO files (id, purpose, page_id, space_id, uploader_id, name, mime, size, storage_key) VALUES ($1,'page',$2,$3,$4,$5,$6,$7,$8)` (`routes/files.rs:111`);
  image inserts with a generated id: `INSERT INTO files (purpose, space_id, uploader_id, name, mime, size, storage_key) VALUES ($1..$7)` (`files.rs:137`).
- **Q139** `duplicate.rs:228` — bulk insert: `INSERT INTO files (id, purpose, page_id, space_id, uploader_id, name, mime, size, storage_key) SELECT r.id, 'page', r.page_id, $1::uuid, $2::uuid, r.name, r.mime, r.size, r.storage_key FROM jsonb_to_recordset($3::jsonb) AS r(id uuid, page_id uuid, name text, mime text, size bigint, storage_key text)`.
- **Q140** by id: `SELECT id, page_id, space_id, name, mime, size, storage_key, updated_at FROM files WHERE id = $1 AND purpose = 'page'` (`routes/files.rs:137`, every download);
  `SELECT id, name, storage_key FROM files WHERE id = $1 AND purpose = 'page' AND space_id = $2` (`routes/files.rs:63`).
- **Q141** by page(s): `SELECT f.id, f.name, f.mime, f.size, f.created_at, f.uploader_id FROM files f WHERE f.page_id = $1 AND f.purpose = 'page' ORDER BY f.created_at DESC LIMIT 500` (`routes/pages.rs:500`);
  `SELECT id, page_id, name, mime, size, storage_key FROM files WHERE purpose = 'page' AND page_id = ANY($1::uuid[])` (`duplicate.rs:60`);
  `SELECT id, name, size, storage_key FROM files WHERE page_id = ANY($1::uuid[])` (`export.rs:423`).
- **Q142** `routes/files.rs:79` — **replace a file's object and get back the key it superseded, safely under
  concurrency** (lock in a subselect, update, return the pre-update value; "concurrent replacements each remove
  the object they superseded"):
  ```sql
  UPDATE files f SET size = $1, mime = $2, storage_key = $3, updated_at = now()
  FROM (SELECT id, storage_key FROM files WHERE id = $4 FOR UPDATE) prev
  WHERE f.id = prev.id
  RETURNING prev.storage_key AS old_key
  ```
- **Q143** `files.rs:155` — `DELETE FROM files WHERE storage_key = $1` (lookup by a non-indexed, non-unique column).
  (Deletes of page files happen inside Q71 and Q108.)

### 2.19 `shares`

- **Q144** `shares.rs:13` — share joined to its page: `SELECT sh.id, sh.key, sh.page_id, sh.include_children, sh.search_indexing, sh.created_at, sp.title AS page_title, sp.slug_id AS page_slug_id FROM shares sh JOIN pages sp ON sp.id = sh.page_id` + `WHERE sh.page_id = $1` (`routes/shares.rs:122`, `:176`).
- **Q145** `shares.rs:80` — **public entry point**: share by key, only if its page is live and sharing is enabled
  at both levels (three-table join + F7):
  `SELECT …Q144 columns…, sp.space_id FROM shares sh JOIN pages sp ON sp.id = sh.page_id JOIN spaces s ON s.id = sp.space_id WHERE sh.key = $1 AND sp.deleted_at IS NULL AND «F7 sharing_enabled(s)»`.
- **Q146** `routes/shares.rs:193` — shares of a space: `SELECT sh.id AS share_id, sh.key, sh.include_children, sh.search_indexing, sh.created_at AS share_created_at, «F1» FROM shares sh JOIN pages p ON p.id = sh.page_id WHERE p.space_id = $1 AND p.deleted_at IS NULL ORDER BY sh.created_at DESC`.
- **Q147** `routes/shares.rs:160` — **create-or-patch upsert**: insert with defaults, or on conflict change only the
  flags that were actually given (the same parameters are bound twice, once as values and once as nullable overrides):
  ```sql
  INSERT INTO shares (key, page_id, include_children, search_indexing, created_by) VALUES ($1, $2, $3, $4, $5)
  ON CONFLICT (page_id) DO UPDATE SET
    include_children = COALESCE($6::boolean, shares.include_children),
    search_indexing  = COALESCE($7::boolean, shares.search_indexing),
    updated_at = now()
  ```
- **Q148** deletes: `DELETE FROM shares WHERE page_id = $1` (`routes/shares.rs:182`);
  by subquery `DELETE FROM shares WHERE page_id IN (SELECT id FROM pages WHERE space_id = $1)` (`routes/spaces.rs:146`);
  by recursive subtree inside Q106 and Q109.

### 2.20 `favorites`, `watchers`

- **Q149** `routes/favorites.rs:16` — favourite pages in the order they were added, permission-filtered:
  `SELECT «F1», «F5» AS space FROM favorites f JOIN pages p ON p.id = f.page_id JOIN spaces s ON s.id = p.space_id WHERE f.user_id = $1 AND p.deleted_at IS NULL AND p.space_id IN «F4» ORDER BY f.created_at`
  (favourite spaces: see Q64).
- **Q150** idempotent adds on **partial unique indexes**:
  `INSERT INTO favorites (user_id, page_id) VALUES ($1, $2) ON CONFLICT (user_id, page_id) WHERE page_id IS NOT NULL DO NOTHING` (`routes/favorites.rs:54`);
  `INSERT INTO favorites (user_id, space_id) VALUES ($1, $2) ON CONFLICT (user_id, space_id) WHERE space_id IS NOT NULL DO NOTHING` (`:64`);
  `INSERT INTO watchers (user_id, space_id) VALUES ($1, $2) ON CONFLICT (user_id, space_id) WHERE space_id IS NOT NULL DO NOTHING` (`routes/watch.rs:57`).
- **Q151** deletes by natural key: `DELETE FROM favorites WHERE user_id = $1 AND page_id = $2` / `… AND space_id = $2`
  (`routes/favorites.rs:83`, `:85`); `DELETE FROM watchers WHERE user_id = $1 AND page_id = $2` / `… AND space_id = $2` (`routes/watch.rs:40`, `:65`).
- **Q152** existence: `SELECT 1 AS one FROM watchers WHERE user_id = $1 AND page_id = $2` / `… AND space_id = $2` (`routes/watch.rs:30`, `:48`).
- **Q153** `notifications.rs:133` — make several users watch a page (after every save, page create, comment):
  `INSERT INTO watchers (user_id, page_id) SELECT id, $1 FROM users WHERE id = ANY($2::uuid[]) ON CONFLICT (user_id, page_id) WHERE page_id IS NOT NULL DO NOTHING`.
- **Q154** `duplicate.rs:271` — one user watches many pages (array expanded to rows):
  `INSERT INTO watchers (user_id, page_id) SELECT $1::uuid, unnest($2::uuid[]) ON CONFLICT (user_id, page_id) WHERE page_id IS NOT NULL DO NOTHING`.
- **Q155** `routes/comments.rs:96` — `SELECT user_id AS id FROM watchers WHERE page_id = $1`.
- **Q156** `pages.rs:366` — who to tell about an edit: watchers of the page **or** of its space, minus the editors,
  minus anyone already notified about this page in the last 7 hours (anti-join on `notifications` with an
  interval parameter):
  ```sql
  SELECT DISTINCT w.user_id FROM watchers w
  WHERE (w.page_id = $1 OR w.space_id = $2)
    AND NOT (w.user_id = ANY($3::uuid[]))
    AND NOT EXISTS (SELECT 1 FROM notifications n
                    WHERE n.user_id = w.user_id AND n.page_id = $1 AND n.kind = 'page.updated'
                      AND n.created_at > now() - $4::interval)          -- '7 hours'
  ```

### 2.21 `labels`, `page_labels`

- **Q157** `routes/labels.rs:32` — `SELECT l.id, l.name FROM page_labels pl JOIN labels l ON l.id = pl.label_id WHERE pl.page_id = $1 ORDER BY l.name`.
- **Q158** `routes/labels.rs:59` — get-or-create many by unique name: `INSERT INTO labels (name) SELECT unnest($1::text[]) ON CONFLICT (name) DO NOTHING`;
  then `routes/labels.rs:60` — `INSERT INTO page_labels (page_id, label_id) SELECT $1, id FROM labels WHERE name = ANY($2::text[]) ON CONFLICT DO NOTHING`.
- **Q159** `routes/labels.rs:77` — `DELETE FROM page_labels WHERE page_id = $1 AND label_id = $2`; then **garbage-collect
  the label if unused** (`:78`): `DELETE FROM labels l WHERE l.id = $1 AND NOT EXISTS (SELECT 1 FROM page_labels WHERE label_id = l.id)`.
- **Q160** `routes/labels.rs:85` — the one **GROUP BY**: labels with the number of readable, live pages, most used first:
  ```sql
  SELECT l.id, l.name, count(p.id)::int AS page_count
  FROM labels l JOIN page_labels pl ON pl.label_id = l.id
  JOIN pages p ON p.id = pl.page_id AND p.deleted_at IS NULL AND p.space_id IN «F4»
  WHERE $1::text = '' OR l.name LIKE $2          -- '%escaped(q)%', case-sensitive
  GROUP BY l.id ORDER BY count(p.id) DESC, l.name LIMIT 200
  ```
- **Q161** `routes/labels.rs:107` — pages of a label: `SELECT «F1», «F5» AS space FROM page_labels pl JOIN pages p ON p.id = pl.page_id JOIN spaces s ON s.id = p.space_id WHERE pl.label_id = $1 AND p.deleted_at IS NULL AND p.space_id IN «F4» ORDER BY p.updated_at DESC, p.id LIMIT 500`.

### 2.22 `notifications`

- **Q162** `notifications.rs:60` — **fan-out insert with the permission check inside the statement**, returning
  what the app needs to push events and send mail (CTE → INSERT … SELECT … RETURNING → join):
  ```sql
  WITH target AS (SELECT space_id FROM pages WHERE id = $1 AND deleted_at IS NULL),
  inserted AS (
    INSERT INTO notifications (user_id, kind, actor_id, page_id, comment_id, data)
    SELECT u.id, $2::text, $3::uuid, $1::uuid, $4::uuid, '{}'::jsonb
    FROM users u, target t
    WHERE u.id = ANY($5::uuid[]) AND u.deactivated_at IS NULL
      AND EXISTS (SELECT 1 FROM space_members sm WHERE sm.space_id = t.space_id
                  AND (sm.user_id = u.id OR sm.group_id IN (SELECT gm.group_id FROM group_members gm WHERE gm.user_id = u.id)))
    RETURNING id, user_id)
  SELECT i.id, i.user_id, u.email, u.preferences FROM inserted i JOIN users u ON u.id = i.user_id
  ```
- **Q163** `notifications.rs:112` — `UPDATE notifications SET emailed_at = now() WHERE id = ANY($1::uuid[])`.
- **Q164** `routes/notifications.rs:19` — the inbox: four LEFT JOINs, one of them carrying the permission check in
  its ON clause (a page the user can no longer read shows as `null`), three JSON objects built per row, optional
  unread filter, offset pagination:
  ```sql
  SELECT n.id, n.kind, n.data, n.created_at, n.read_at, «F6 user_ref_json(a)» AS actor,
    CASE WHEN p.id IS NULL THEN NULL ELSE json_build_object('id', p.id, 'slugId', p.slug_id, 'title', p.title,
      'icon', p.icon, 'spaceId', p.space_id, 'spaceSlug', s.slug) END AS page,
    CASE WHEN c.id IS NULL OR p.id IS NULL THEN NULL ELSE json_build_object('id', c.id, 'parentId', c.parent_id) END AS comment
  FROM notifications n
  LEFT JOIN users a ON a.id = n.actor_id
  LEFT JOIN pages p ON p.id = n.page_id AND p.deleted_at IS NULL AND p.space_id IN «F4»
  LEFT JOIN spaces s ON s.id = p.space_id
  LEFT JOIN comments c ON c.id = n.comment_id
  WHERE n.user_id = $1 AND (NOT $2 OR n.read_at IS NULL)
  ORDER BY n.created_at DESC, n.id LIMIT $3 OFFSET $4
  ```
- **Q165** `routes/notifications.rs:58` — badge count (polled by the client): `SELECT count(*)::int AS count FROM notifications WHERE user_id = $1 AND read_at IS NULL`.
- **Q166** mark read: `UPDATE notifications SET read_at = now() WHERE user_id = $1 AND id = ANY($2::uuid[]) AND read_at IS NULL` (`routes/notifications.rs:65`);
  all: `… WHERE user_id = $1 AND read_at IS NULL` (`:77`).

Notifications are never deleted except by cascade.

### 2.23 `base_properties`, `base_views` (schema and views of a base)

- **Q167** `bases.rs:106` — all properties in column order (run at the start of **every** base endpoint):
  `SELECT id, name, type, options, is_primary, position FROM base_properties WHERE page_id = $1 ORDER BY position, id`.
- **Q168** `routes/bases.rs:519` — "has any": `SELECT 1 AS one FROM base_properties WHERE page_id = $1 LIMIT 1`.
- **Q169** inserts: `INSERT INTO base_properties (page_id, id, name, type, options, is_primary, position) VALUES ($1,$2,$3,$4,$5::jsonb,$6,$7)` (`bases.rs:174`, seeding, 3–4 rows one by one);
  `INSERT INTO base_properties (page_id, id, name, type, options, position) VALUES (…)` (`routes/bases.rs:742`; unique violation on the name key → 400).
- **Q170** `duplicate.rs:172` — copy all properties of several bases at once through an old→new id map:
  `INSERT INTO base_properties (page_id, id, name, type, options, is_primary, position) SELECT m.new, bp.id, bp.name, bp.type, bp.options, bp.is_primary, bp.position FROM jsonb_to_recordset($1::jsonb) AS m(old uuid, new uuid) JOIN base_properties bp ON bp.page_id = m.old`.
- **Q171** updates by composite key: `UPDATE base_properties SET name = $1, type = $2, options = $3::jsonb, position = $4, updated_at = now() WHERE page_id = $5 AND id = $6` (`routes/bases.rs:826`; unique violation → 400);
  `UPDATE base_properties SET options = $1::jsonb, updated_at = now() WHERE page_id = $2 AND id = $3` (`:850`, once per dependent formula on rename).
- **Q172** `routes/bases.rs:909` — `DELETE FROM base_properties WHERE page_id = $1 AND id = $2`.
- **Q173** `bases.rs:116` — views in order: `SELECT id, name, type, config, position FROM base_views WHERE page_id = $1 ORDER BY position, id`;
  one view: `… WHERE page_id = $1 AND id = $2` (`routes/bases.rs:958`).
- **Q174** `routes/bases.rs:931` — `SELECT max(position) AS position FROM base_views WHERE page_id = $1` (append key).
- **Q175** inserts: `INSERT INTO base_views (page_id, name, type, position, creator_id) VALUES ($1,'Table','table',$2,$3)` (`bases.rs:193`);
  `… (page_id, name, type, config, position, creator_id) VALUES ($1,'Board','kanban',$2::jsonb,$3,$4)` (`bases.rs:203`);
  `… VALUES ($1,$2,$3,$4::jsonb,$5,$6) RETURNING id, name, type, config, position` (`routes/bases.rs:934`).
- **Q176** `duplicate.rs:187` — `INSERT INTO base_views (page_id, name, type, config, position, creator_id) SELECT m.new, bv.name, bv.type, bv.config, bv.position, $1::uuid FROM jsonb_to_recordset($2::jsonb) AS m(old uuid, new uuid) JOIN base_views bv ON bv.page_id = m.old`.
- **Q177** updates: `UPDATE base_views SET name = $1, type = $2, config = $3::jsonb, position = $4, updated_at = now() WHERE id = $5 RETURNING id, name, type, config, position` (`routes/bases.rs:969`; read-modify-write of `config` without a lock);
  `UPDATE base_views SET config = $1::jsonb, updated_at = now() WHERE id = $2` (`:340`, per view when a property is deleted).
- **Q178** `routes/bases.rs:993` — **lock every view of a base** to enforce "a base keeps at least one view":
  `SELECT id FROM base_views WHERE page_id = $1 FOR UPDATE`; then `DELETE FROM base_views WHERE id = $1` (`:1000`).

### 2.24 `base_rows` — point access and writes

Row columns **R** (`bases.rs:101`): `r.id, r.cells, r.position, r.creator_id, r.last_editor_id, r.created_at, r.updated_at`.

- **Q179** `routes/bases.rs:628` — `SELECT «R» FROM base_rows r WHERE r.id = $1 AND r.page_id = $2`;
  with a row lock for read-modify-write: `… FOR UPDATE` (`:651`).
- **Q180** position neighbours (inside the insert transaction): `SELECT position FROM base_rows WHERE id = $1 AND page_id = $2` (`routes/bases.rs:579`);
  `SELECT min(position) AS position FROM base_rows WHERE page_id = $1 AND position > $2` (`:583`);
  `SELECT max(position) AS position FROM base_rows WHERE page_id = $1` (`:590`).
- **Q181** inserts: `INSERT INTO base_rows (page_id, position, creator_id, last_editor_id) VALUES ($1,$2,$3,$4)` (`bases.rs:190`);
  `INSERT INTO base_rows (page_id, cells, position, creator_id, last_editor_id) VALUES ($1,$2::jsonb,$3,$4,$5) RETURNING id, cells, position, creator_id, last_editor_id, created_at, updated_at` (`routes/bases.rs:601`).
- **Q182** `routes/bases.rs:677` — **partial JSON update of one row** through `apply_cell_patch` (null removes a
  key), optional position change, returning the new row:
  ```sql
  UPDATE base_rows SET cells = apply_cell_patch(cells, $1::jsonb), updated_at = now(), last_editor_id = $2,
    position = coalesce($3::text, position)
  WHERE id = $4
  RETURNING id, cells, position, creator_id, last_editor_id, created_at, updated_at
  ```
- **Q183** `routes/bases.rs:218` — **many rows, a different patch each, one statement** (`{rowId: patch}` object
  expanded with `jsonb_each` and joined to the table); used by formula back-fill and type conversion in batches of ≤ 500:
  ```sql
  UPDATE base_rows r SET cells = apply_cell_patch(r.cells, p.value)
  FROM jsonb_each($1::jsonb) p
  WHERE r.page_id = $2 AND r.id = p.key::uuid
  ```
  (does not touch `updated_at`.)
- **Q184** `routes/bases.rs:910` — remove one key from every row that has it (JSON key-exists predicate + key removal):
  `UPDATE base_rows SET cells = cells - $1::text WHERE page_id = $2 AND cells ? $1::text`.
- **Q185** `routes/bases.rs:713` — bulk delete, **returning which ids really existed in this base**:
  `DELETE FROM base_rows WHERE page_id = $1 AND id = ANY($2::uuid[]) RETURNING id`.
- **Q186** `duplicate.rs:179` — copy rows of several bases, keeping `created_at`: `INSERT INTO base_rows (page_id, cells, position, creator_id, last_editor_id, created_at, updated_at) SELECT m.new, br.cells, br.position, br.creator_id, $1::uuid, br.created_at, now() FROM jsonb_to_recordset($2::jsonb) AS m(old uuid, new uuid) JOIN base_rows br ON br.page_id = m.old`.
- **Q187** `duplicate.rs:200` — find rows whose cells mention any of a set of file ids (**regex over the JSON text**):
  `SELECT id, cells FROM base_rows WHERE page_id = ANY($1::uuid[]) AND cells::text ~ $2` (`$2` = `uuid1|uuid2|…`);
  then write them back in bulk (`duplicate.rs:217`): `UPDATE base_rows br SET cells = u.cells FROM jsonb_to_recordset($1::jsonb) AS u(id uuid, cells jsonb) WHERE br.id = u.id`.

### 2.25 `base_rows` — the dynamic row query (filters, sorts, pagination)

`POST /bases/:id/rows/query`, public shares of bases, CSV export and Markdown/HTML export all go through
`query_rows`/`all_rows` (`bases.rs:662-768`). The WHERE and ORDER BY are **compiled at run time from a
user-supplied filter tree and sort list** (`compile_filter` `bases.rs:500`, `compile_sorts` `bases.rs:575`).

- **Q188** `bases.rs:705` — unsorted: **keyset pagination** on `(position, id)` with a row-value comparison in
  bytewise collation; fetches `limit + 1` to learn whether there is a next page:
  ```sql
  SELECT «R» FROM base_rows r
  WHERE r.page_id = $1 AND «filter» AND (r.position, r.id) > ($2::text COLLATE "C", $3::uuid)   -- or `true` on the first page
  ORDER BY r.position, r.id LIMIT $4
  ```
  The cursor is opaque base64url JSON `{"p": position, "i": id}`. `bases.rs:748` is the same without a filter,
  in batches of 500, for exports, back-fills and conversions (also run *inside* transactions).
- **Q189** `bases.rs:680` — sorted: **offset pagination** ("because the sort keys are nullable expressions"),
  cursor `{"o": offset}`, with `(position, id)` as the final tie-break:
  ```sql
  SELECT «R» FROM base_rows r WHERE r.page_id = $1 AND «filter»
  ORDER BY «sort₁» ASC|DESC NULLS LAST, …, r.position, r.id LIMIT $2 OFFSET $3
  ```

**Column expression** per property (`column_expr`, `bases.rs:233`), by "column kind":

| kind (property types) | expression over row `r` |
|---|---|
| text (text, longText, url, email, select, status, single person, page, text formulas, …) | `(r.cells ->> $id::text)` — JSON value as text; non-strings read as their JSON text |
| number (number, number formulas) | `cell_number((r.cells -> $id::text))` → `numeric` |
| time (date, date formulas) | `cell_time((r.cells -> $id::text), $tzOffset::int)` → `timestamptz` |
| time (createdAt / lastEditedAt) | `r.created_at` / `r.updated_at` |
| flag (checkbox, boolean formulas) | `cell_flag((r.cells -> $id::text))` → `boolean` |
| list (multiSelect, file, multi-person) | `cell_list((r.cells -> $id::text))` → `jsonb` array |
| lastEditedBy | `coalesce(r.last_editor_id, r.creator_id)::text` |

**Filter conditions** (`compile_condition`, `bases.rs:301-433`); groups nest with `AND`/`OR` to depth 10, up to
200 children each; an empty group is `true`. `E` = the column expression:

| kind | operator → SQL |
|---|---|
| any | `isEmpty` → text: `(E IS NULL OR E = '')`; list: `coalesce(jsonb_array_length(E), 0) = 0`; others: `E IS NULL`. `isNotEmpty` → `NOT (…)` |
| text | `eq` `E = $v` · `neq` `E IS DISTINCT FROM $v` · `contains` `E ILIKE '%v%'` · `ncontains` `NOT coalesce(E ILIKE '%v%', false)` · `startsWith` `E ILIKE 'v%'` · `endsWith` `E ILIKE '%v'` · `gt/gte/lt/lte` `E > $v` … (**text comparison in the database collation**) · `any` `E = ANY($list::text[])` · `none` `(E IS NULL OR NOT (E = ANY($list::text[])))` · `all` (one item) `E = $v` |
| number | `E = $n::numeric`, `E IS DISTINCT FROM $n::numeric`, `>`, `>=`, `<`, `<=` (exact decimal comparison) |
| flag | `eq` `coalesce(E, false) = true` or `coalesce(E, false) = false` (by the truthiness of the value); `neq` wraps it in `NOT (…)` |
| time | the app turns the value (a date, or a relative token such as `today`, `pastWeek`, `thisMonth`) into a half-open UTC range `[start, end)` and sends ISO strings: `eq/isWithin` `(E >= $start::timestamptz AND E < $end::timestamptz)` · `neq` `NOT coalesce((…), false)` · `before/lt` `E < $start` · `onOrBefore/lte` `E < $end` · `after/gt` `E >= $end` · `onOrAfter/gte` `E >= $start` |
| list | `eq` `coalesce(E ? $v, false)` · `neq` `NOT coalesce(E ? $v, false)` · `any` `coalesce(E ?\| $list::text[], false)` · `none` `NOT coalesce(E ?\| $list::text[], false)` · `all` `coalesce(E ?& $list::text[], false)` · `contains` `coalesce(E::text ILIKE '%v%', false)` (substring of the **JSON text of the array**) · `ncontains` negated |

`%`, `_` and `\` in user text are escaped by the app (`search.rs:49`) and the default `\` escape of `LIKE` is relied on.

Degenerate values are decided by the app and compiled to constants: an empty list gives `false` for `any`/`eq`
and `true` for `none`/`all`/`neq`; a non-finite number gives `true` for `neq` and `false` otherwise; a date value
that yields no day gives `false` for every operator. Relative dates (`today`, `pastWeek`, …) are resolved with the
**application server's clock** and the request's `tzOffset`, not with `now()`. An unknown property id or an
operator not allowed for the kind is a 400 before any SQL runs.

**Sort expressions** (`sort_expr`, `bases.rs:553`), each `ASC NULLS LAST` or `DESC NULLS LAST` (empty cells last
either way), up to 10:

| property | expression |
|---|---|
| select / status | `array_position($choiceOrder::text[], E)` — order of the choice in the property's option list |
| lastEditedBy | `(SELECT lower(u.name) FROM users u WHERE u.id = coalesce(r.last_editor_id, r.creator_id))` — **correlated lookup in another table as a sort key** |
| single person | `(SELECT lower(u.name) FROM users u WHERE u.id = CASE WHEN E ~* '^[0-9a-f]{8}-…-[0-9a-f]{12}$' THEN (E)::uuid END)` |
| list kinds | `E::text` (the JSON text of the array) |
| everything else | `E` (numeric, timestamptz, boolean or collated text order) |

### 2.26 `realtime_leader` and totals

- **Q190** `realtime/leader.rs:99` — **lease claim/renew as one conditional upsert on the database clock**; a
  returned row means "you lead", no row means "someone else holds an unexpired lease":
  ```sql
  INSERT INTO realtime_leader (holder, expires_at) VALUES ($1, now() + $2::float8 * interval '1 second')
  ON CONFLICT (id) DO UPDATE SET holder = excluded.holder, expires_at = excluded.expires_at
  WHERE realtime_leader.holder = excluded.holder OR realtime_leader.expires_at < now()
  RETURNING holder
  ```
- **Q191** `realtime/leader.rs:160` — release: `DELETE FROM realtime_leader WHERE holder = $1`.

**Totals:** 191 numbered shapes (Q1–Q191), covering all 267 complete statement texts in the Rust server (many
shapes group several call sites or column variants), plus 7 shared fragments (F1–F7) and the run-time filter/sort grammar of §2.25 (≈ 45 further predicate
and ordering forms). Not used anywhere: `TRUNCATE`, `LISTEN`/`NOTIFY`, `COPY`, cursors, `SAVEPOINT`,
`SELECT … SKIP LOCKED`/`NOWAIT`, `HAVING`, `INTERSECT`/`EXCEPT`, `FULL`/`RIGHT`/`CROSS JOIN` (one implicit cross
join against a single-row CTE in Q162), `SUM`/`AVG`, sequences, materialised views, stored procedures beyond the
five functions of §1.27. Aggregates used: `count(*)`, `count(col)`, `min`, `max`, `count(*) OVER ()`,
`jsonb_object_agg` (inside `apply_cell_patch`).

---
## 3. Transactions and concurrency

### 3.1 Isolation level and general model

- **No isolation level is ever set**: every transaction runs at PostgreSQL's default **READ COMMITTED**. No
  `SET TRANSACTION`, no `SERIALIZABLE`/`REPEATABLE READ`, no `SAVEPOINT`, no session settings (`SET …`), no
  statement/lock/idle timeouts. Serialization failures (`40001`) are therefore never expected and never handled.
- Correctness under concurrency comes from five mechanisms, all listed below: explicit row locks
  (`FOR UPDATE`), transaction-scoped advisory locks, unique constraints (+ `ON CONFLICT`), conditional writes
  whose `RETURNING` set tells who won, and an application-level optimistic version check on `pages.doc_version`.
- Statement-level atomicity is leaned on heavily: multi-table changes are often packed into **one statement
  with data-modifying CTEs** (Q71, Q106–Q109, Q162) instead of a transaction.
- Inside a transaction the code sometimes reads **through the pool on another connection** (i.e. outside the
  transaction, seeing only committed data): `row_references` → Q15 and `resolve_pages` → Q92 during
  `convert_rows` (`routes/bases.rs:271`, `:275`). With the default pool of 5 this also means one request can hold
  two connections.
- A request that has started is run to its end even if the client disconnects (`main.rs:117-125`), so the
  database never sees a half-run handler because of a dropped connection.

### 3.2 Every multi-statement transaction (25)

| # | where | what it groups |
|---|---|---|
| T1 | `db.rs:726` migrate (per migration) | Q6 advisory lock 726354 → Q3 re-check → Q5 whole migration file (DDL) → Q4 record it |
| T2 | `routes/auth.rs:50` first-run setup | Q6 lock 4815162342 → Q19 "any users?" → Q25 insert owner → `init_workspace`: Q9 upsert workspace, Q50 group `Everyone`, Q56 membership, Q68 space `General`, Q75 admin membership, Q10 default space |
| T3 | `sso.rs:224` first SSO sign-in | Q6 same lock → Q16 by `auth_sub` → Q18 email taken? → Q19 → Q25 insert → Q57 join default groups, or `init_workspace` as in T2 |
| T4 | `routes/invitations.rs:210` accept invitation | Q63 `DELETE … RETURNING` (claim once) → Q18 → Q25 insert user → Q57 group memberships |
| T5 | `routes/members.rs:86` change workspace role | Q6 lock 1618033988 → Q24 count active owners ("last owner cannot be demoted") → Q28 |
| T6 | `routes/spaces.rs:77` create space | Q68 insert space → Q75 creator as admin |
| T7 | `routes/spaces.rs:128` update space | Q69 → (if sharing was just disabled) Q148 delete all shares of the space |
| T8 | `routes/spaces.rs:257` add members | Q76 upsert users → Q76 upsert groups |
| T9 | `routes/spaces.rs:299` change member role | Q77 `FOR UPDATE` → Q78 count admins ("last admin cannot be demoted") → Q79 update |
| T10 | `routes/spaces.rs:325` remove member | Q77 `FOR UPDATE` → Q78 → Q79 delete |
| T11 | `routes/labels.rs:58` add labels | Q158 get-or-create labels → attach |
| T12 | `routes/pages.rs:199` create page | Q84 parent check → Q95–Q97 neighbour keys → Q110 insert → Q123/Q124/Q127/Q128 index links and synced blocks |
| T13 | `routes/pages.rs:296` move page | Q84 → Q99 cycle check → Q95–Q97 → Q112 |
| T14 | `routes/pages.rs:326` move to another space | Q95 → Q109 |
| T15 | `pages.rs:453` restore from trash | Q116 `FOR UPDATE OF p` → (parent gone/trashed) Q95 + Q112 → Q107 |
| T16 | **`pages.rs:303` save page content** (hot) | Q114 `FOR UPDATE` → version check in the app → Q115 → Q123, Q124, Q127, Q128 → Q120 conditional snapshot |
| T17 | `duplicate.rs:148` duplicate subtree | Q95–Q97 → Q117 pages → Q170, Q186, Q176 base data → Q187 (read + bulk rewrite of file cells) → Q139 files → Q125 links → Q129 synced blocks → Q154 watchers |
| T18 | `routes/bases.rs:450` create base | Q84 → Q95–Q97 → Q110 → `seed_base`: 3–4× Q169, Q181, 1–2× Q175 |
| T19 | `routes/bases.rs:518` convert page to base | Q168 "already has base data?" → Q113 → `seed_base` |
| T20 | `routes/bases.rs:576` create row | Q180 neighbour keys → Q181 insert `RETURNING` |
| T21 | `routes/bases.rs:649` update row | Q179 `FOR UPDATE` → diff + formula recompute in the app → Q182 `RETURNING` |
| T22 | `routes/bases.rs:741` create property | Q169 → (formula) full-table back-fill: loop of Q188-batches + Q183 |
| T23 | `routes/bases.rs:824` update property | Q171 → per renamed-formula Q171 → type conversion / option removal: loop of Q188-batches (+ Q15/Q92 on the pool) + Q183 → formula back-fill as T22 |
| T24 | `routes/bases.rs:908` delete property | Q172 → Q184 strip the key from all rows → Q173 + Q177 per view → back-fill of dependent formulas |
| T25 | `routes/bases.rs:992` delete view | Q178 lock all views of the base → count check in the app → delete |

T22–T24 can rewrite **every row of a base in one transaction** (bounded only by the size of the base), in
500-row batches read by keyset inside the same transaction.

Deliberately *not* transactional (sequences of autocommit statements with compensation or tolerated gaps):
password reset (Q47 consume → Q27 → Q46 → Q42 → Q37), forgot-password (Q46 → Q45), sign-in (Q37 → Q26), delete
space (Q72, Q73 then the single-statement Q71), remove label (Q159, two statements), file upload (object written
first, then Q138; the object is removed if the insert fails), image upload (Q138, then Q12/Q31/Q70, then Q143
for the old image), everything after the save commit (Q153, Q162, Q156, Q162, Q163 — failures there are logged
and must not fail the save, `pages.rs:352-395`), comment notifications (spawned after the response).

### 3.3 Row locks, advisory locks, lease

| mechanism | where | purpose / required semantics |
|---|---|---|
| `SELECT … FOR UPDATE` on one `pages` row | Q114 `pages.rs:304` | Serialises saves of one page and makes the version check + write atomic. The API suite holds this very lock from a second connection and expects the server to **block, then proceed** (`server-process.test.ts:267`, `leader.test.ts:138`, `operations.slow.test.ts:214`). |
| `FOR UPDATE OF p` with a LEFT JOIN | Q116 `pages.rs:455` | Lock only the page, not the joined parent. |
| `FOR UPDATE` on one `base_rows` row | Q179 `routes/bases.rs:651` | Read-modify-write of a row's cells without lost updates (two editors patching different cells of one row both win). |
| `FOR UPDATE` on **all** `base_views` rows of a base | Q178 `routes/bases.rs:993` | "At least one view remains": concurrent deletes of the last two views are serialised by locking the set. |
| `FOR UPDATE` on one `space_members` row | Q77 `routes/spaces.rs:300,326` | Guards the role check of that row. (The admin count Q78 that follows only locks this one row, so two *different* admins removed at the same moment do not appear to be protected from both passing the check.) |
| `FOR UPDATE` in a subselect of an UPDATE | Q142 `routes/files.rs:81` | Under READ COMMITTED the subselect waits for a concurrent replacement and then sees its committed key, so each replacement gets the key it really superseded. |
| `pg_advisory_xact_lock(726354)` | T1 | One copy migrates at a time; the others wait, then re-check. (Creating `schema_migrations` itself is outside the lock: two copies starting at the same instant on a completely empty database can collide and one exits — pinned as a known bug in `server-process.test.ts:96`.) |
| `pg_advisory_xact_lock(4815162342)` | T2, T3 | The workspace is set up exactly once even when two first sign-ins race (they would otherwise both see "no users"). A mutex with no row to hang it on. |
| `pg_advisory_xact_lock(1618033988)` | T5 | Serialises all owner-role changes so "count owners, then demote" cannot leave zero owners. |
| lease row `realtime_leader` | Q190/Q191 | See §4.1. Chosen *instead of* an advisory lock "because a lock goes with its connection, and the holder would only learn that at its next query" (`realtime/leader.rs:8-12`). |

No `SKIP LOCKED`, no `NOWAIT`, no `LOCK TABLE` in the app (the test-suite uses `LOCK TABLE pages IN ACCESS
EXCLUSIVE MODE` from outside to stall the server, `realtime-save.test.ts:899`).

### 3.4 Errors the app catches by SQLSTATE

Only two codes are inspected (`db.rs:46-53`; TS checks `error.errno`):

| SQLSTATE | where caught | constraint that fires | what the app does |
|---|---|---|---|
| `23505` unique_violation | `routes/spaces.rs:98`, `:153` | `spaces_slug_key (lower(slug))` | 409 `slug_taken` |
| | `routes/groups.rs:74`, `:104` | `groups_name_key (lower(name))` | 409 `name_taken` |
| | `routes/bases.rs:762`, `:888` | `base_properties_name_key (page_id, lower(btrim(name)))` | 400 "A property with this name already exists" (the app pre-checks with its own case-folding and trimming, which differs slightly from `lower(btrim())`, so the constraint is the real arbiter) |
| | `sso.rs:214` | `users_auth_sub_key` | another sign-in of the same person linked first → re-read by `auth_sub` |
| | `sso.rs:264` | `users_email_key` or `users_auth_sub_key` during T3 | re-read by `auth_sub`, else answer `account_exists` |
| | `sso.rs:355` | `users_email_key` on Q33 | **retry the same UPDATE without the email** (the account keeps its own address) |
| `40P01` deadlock_detected | `sso.rs:319` | two sign-ins of one person each updating two `users` rows in opposite order | **retry the whole resolve up to 3 times** (`RESOLVE_TRIES`). The comment and `sso-users.test.ts:475-494` describe the exact interleaving; the test provokes it with a trigger. |

The catch is per transaction, not per constraint: *any* unique violation inside T6/T7/T22/T23 is reported as the
slug/name being taken.

Everything else surfaces as HTTP 500 `internal_error`, and the API suite **pins several of those 500s as current
behaviour** (they come from PostgreSQL's own input validation):

- a NUL character in any text or JSON value (class-22 data exceptions such as `22021`, `22P05`): `pages.test.ts:303`, `page-content.test.ts:692`,
  `comments.test.ts:196`, `spaces.test.ts:924`, `http.test.ts:897`, `base-cells.test.ts:134`;
- a lone UTF-16 surrogate inside JSON (`\ud83d`), refused by jsonb: `page-content.test.ts:704`,
  `base-cells.test.ts:137`, `base-formulas.test.ts:1286`, `base-properties.test.ts:844`, `page-tree.test.ts:837`
  (in plain text columns the Rust server replaces a lone surrogate with U+FFFD before sending, `db.rs:108`);
- a timestamp out of range (year 10007) sent as a filter bound: `base-filters.test.ts:551`;
- unhandled races: email taken between check and write (Q30, T4), `slug_id`/`key` collisions, a foreign key
  failing because a page/space vanished between the access check and the insert (`23503`).

### 3.5 Optimistic concurrency

- **`pages.doc_version`** is the one explicit version check. A collaboration session remembers the stored version
  it loaded (`saved_version`); T16 locks the row, compares `doc_version` with it, and refuses the save (returns
  "conflict", nothing written) if they differ; the session then reloads from the database and resets every
  connected editor (`realtime/collab.rs:168-234`). Normally only the lease holder saves, so this is the backstop
  for a lease that failed. `stored_doc_version` (Q83) is also read without a lock to decide whether an idle
  in-memory session is stale.
- Conditional writes whose result set is the decision (no version column): Q32 (link account if unlinked), Q47
  (consume token), Q63 (claim invitation), Q62 (rotate, 404 when gone), Q142 (swap file), Q185 (which rows were
  deleted), Q43 (which sessions ended), Q190 (lease).
- "Upsert and keep going" instead of check-then-insert: Q9, Q57, Q61, Q76, Q124, Q128, Q147, Q150, Q153, Q154, Q158.

### 3.6 Retries and timeouts

- Database-level: none configured. No query timeout, no pool-acquire timeout in the Rust server (deadpool waits
  indefinitely; `RecyclingMethod::Fast`), TS closes the pool with a 5 s timeout at shutdown.
- Application-level retries that touch the database: the deadlock retry above; a failed page save keeps the
  document dirty in memory and is retried at the next edit or when the page is left
  (`realtime/collab.rs:176-187`); a failed lease renewal is logged and retried at the next tick while the
  in-memory lease deadline still runs (`realtime/leader.rs:114-122`); a renewal that *hangs* past the lease makes
  the copy stop leading on its own timer (`:77-88`).
- Shutdown: stop leading → close sockets → save every open document → release the lease → wait ≤ 10 s for
  in-flight requests → close the pool (`main.rs:149-157`, `realtime/ws.rs:376-392`).

---

## 4. Other server features

### 4.1 LISTEN/NOTIFY, cross-process signalling, the leader lease

- **No `LISTEN`/`NOTIFY`** (and no polling of tables for events). All realtime fan-out (`emit_to_space`,
  `emit_to_user`, `emit_to_page`) is **in-process memory** on the one copy that holds the WebSocket connections
  (`realtime/hub.rs`).
- Cross-process coordination is entirely "state in tables + periodic re-reads":
  - **Leader lease** (Q190): one row, `holder` = a random UUID per process, `expires_at = now() + 10 s`, renewed
    every 3 s by the leader and attempted every 1 s by the others. Expiry is judged by the **database clock**
    (`expires_at < now()`), the holder's own deadline by its monotonic clock measured from *before* the request
    was sent, so the holder always stops believing first. Takeover after a clean shutdown is within ~1 s (lease
    row deleted, Q191), after a crash within 10 s.
  - Sessions ended on another copy are noticed by the leader's once-a-minute Q36 over all open sockets.
  - A page saved by another copy is noticed through `doc_version` (§3.5).
- Requirement for a replacement: either several server processes can open the same database concurrently with
  cross-process atomic compare-and-set and a shared clock, or the rolling-deploy design must change.

### 4.2 Full-text search (how `search.rs` works, exactly)

Indexing (database side)

- `pages.search` is a stored generated `tsvector`: title with weight **A**, first 1 000 000 characters of
  `text_content` with weight **B**, both through the **`simple`** configuration (PostgreSQL default parser, then
  lower-casing only: no stemming, no stop words, no synonyms). GIN-indexed.
- `text_content` is produced by the app on every save (`doc_text`, one line per text block). Titles and text are
  stored as typed (no normalisation); only the *query* is normalised.
- The default parser's token classes are therefore observable: a hyphenated word yields the whole and its parts,
  `user@example.com` and `example.com/path` are single email/URL/host tokens (so `example` does not match by
  word), `apple_pie` is two words, `<b>`-like tags are dropped, numbers are tokens. What counts as a letter for
  non-ASCII text depends on the database locale. The test suite covers Latin, punctuation and Korean
  (space-separated words match by prefix; text without spaces only matches through the substring paths).

Query construction (app side, `search.rs:22-46`, `:155-170`)

1. Trim, cut to 200 characters → `raw`. Empty → no results.
2. Words: NFKC-normalise, lower-case, split on anything that is not a Unicode letter or digit
   (`[^\p{L}\p{N}]+`), cut each to 50 characters, de-duplicate, keep the first 8.
3. tsquery text: every word as a prefix, all required: `w1:* & w2:* & …` (words are letters/digits only, so
   nothing needs escaping); no words → `NULL` (the full-text branch is skipped).
4. `like = '%' || escape(lower(raw)) || '%'` with `\`, `%`, `_` escaped.

Matching and ranking (Q118)

- A page matches when **`vector @@ to_tsquery('simple', q)`** (every word is a prefix of some indexed word in the
  title or text) **OR** `lower(coalesce(title, '')) LIKE like` (the raw query is a substring of the title).
- `titleOnly` swaps the stored vector for `to_tsvector('simple', coalesce(p.title, ''))` computed per row.
- Scope predicate: readable spaces (F4) and optional space, never trashed pages; for public shares the shared
  page or its live subtree (Q105).
- Order: (1) `lower(coalesce(title,'')) = lower(raw)` first (exact title, case aside); (2) **`ts_rank(vector, q)`
  descending** — PostgreSQL's default weights {D 0.1, C 0.2, B 0.4, A 1.0}, no length normalisation, so a title
  hit outranks a text hit and more occurrences rank higher; pages matched only by the title-substring branch
  have rank 0; (3) `updated_at DESC`; (4) `id`. Then `LIMIT/OFFSET` (limit ≤ 50, offset ≤ 10 000; public search:
  20, rate-limited in memory).
- Fallback (Q119), only when Q118 returned nothing, not title-only, offset 0: `text_content ILIKE '%raw%'` — a
  sequential substring scan over the text of every page in scope — ordered by recency, at most 20. This is what
  finds text without word boundaries (CJK) and mid-word pieces.

Snippets and highlighting

- The **database** cuts the snippet: 400 characters of `text_content` starting 100 characters before the first
  occurrence of the first word (`strpos(lower(text_content), word)`; character positions, start clamped to 1; when
  the word does not occur literally, the first 400 characters). Title-only search returns the title.
- The **app** does the rest (`Highlighter`, `search.rs:66-136`): collapse whitespace, pick a ~200 UTF-16-unit
  window around the first match (cut at spaces, `…` at cut ends), HTML-escape, wrap every occurrence of the raw
  query or any word in `<mark>`, extending a match at a word start to the end of the word. If the text snippet has
  no mark and the page has a title, the highlighted title is shown instead.

Other substring searches (no full-text): user suggest (Q23), page-title suggest (Q91), member list (Q21), label
list (Q160), all `lower(col) LIKE '%q%'` with app-side lower-casing of the needle.

### 4.3 Sequences and server-generated ids

- No sequences. UUID v4 via `gen_random_uuid()` column defaults for 18 tables; the app relies on `RETURNING id`
  (or `RETURNING *`) to learn them.
- App-generated: page-file ids and all ids in a duplicated subtree (UUID v4), `slug_id`, share `key`, property
  ids, choice ids, session/reset/invitation tokens, the per-process lease `holder`.
- UUIDs are compared as the `uuid` type: input is accepted in either letter case and returned lower-case. The
  app lower-cases most ids before use, but relies on the type for the rest. `(position, id)` keyset pagination
  needs a **total order on uuid** consistent between the comparison and the ORDER BY.

### 4.4 Server-side time

- All defaults and most writes use `now()`, which in PostgreSQL is the **transaction start time** — constant
  across a statement and across a transaction. Two behaviours depend on that constancy:
  - Q106 stamps a whole trashed subtree with one identical `deleted_at`; Q107 (restore) and Q89 (trash list) then
    find "trashed together" by **timestamp equality at microsecond precision**.
  - T16 compares version age against the same `now()` in several subqueries.
- Time arithmetic in SQL: `now() + make_interval(mins => $n)`, `now() - make_interval(days => $n)`,
  `now() - interval '5 minutes'`, `now() - $1::interval` (`'7 hours'`), `now() + $1::float8 * interval '1 second'`,
  comparisons `expires_at > now()` on every request.
- Precision: stored at microseconds, returned to clients truncated to milliseconds. File ETags use `updated_at`
  in milliseconds.
- The lease uses the database clock as the single authority across processes (§4.1).
- `cell_time` interprets strings without a zone in the **session time zone** of the database connection.

### 4.5 Blobs

- **No `bytea`, no large objects.** Uploaded files, avatars, logos and icons live in a directory or an S3
  bucket; `files` holds metadata and the `storage_key`. Consistency between the two stores is by ordering and
  compensation (write object → insert row; delete row(s) `RETURNING storage_key` → remove objects).
- The **document** is one `jsonb` value per page (`pages.content`), up to 2 000 000 "positions" (roughly
  characters plus two per node; request bodies up to 8 MiB). It is rewritten whole on every save, and copied
  whole into `page_versions`.
- **Collaborative steps are not stored at all** (no Yjs, no step log in the database). The lease holder keeps
  each open document, its version and up to 2000 steps / 2 MiB of step history in memory
  (`common/editor/collab.rs:30-33`) and persists the resulting document (§6).

### 4.6 JSONB — columns, operators, and semantics relied on

Columns: `workspace.settings`, `spaces.settings`, `users.preferences`, `pages.content`, `page_versions.content`,
`comments.body`, `synced_blocks.content`, `notifications.data`, `base_properties.options`, `base_rows.cells`,
`base_views.config`.

| operator / function | where | meaning needed |
|---|---|---|
| `a \|\| b` (shallow object merge) | Q11, Q30, Q69 | patch settings/preferences |
| `jsonb_strip_nulls` | Q30 | a `null` in the patch removes the key |
| `a - 'key'` | Q184 | remove a key |
| `a ? 'key'` | Q184, `apply_cell_patch`, list filters | key exists / string is an element of an array |
| `a ?\| text[]`, `a ?& text[]` | list filters | any / all of the strings are elements |
| `a -> 'key'`, `a ->> 'key'` | F7, Q109, every filter and sort | member as JSON / as text |
| `v #>> '{}'` | `cell_*` functions | scalar JSON value as text |
| `jsonb_typeof`, `jsonb_array_length`, `jsonb_build_array` | `cell_*`, `isEmpty` | type switches |
| `jsonb_each`, `jsonb_object_agg` | Q183, `apply_cell_patch` | iterate / rebuild an object |
| `jsonb_to_recordset(json) AS r(col type, …)` | Q117, Q125, Q128–Q130, Q139, Q170, Q176, Q186, Q187 | **a JSON array of objects as a typed row set** — the app's bulk-parameter mechanism (including `text[]` and `jsonb` columns) |
| `json_build_object` | F5, F6, Q74, Q164 | nested objects in results |
| `IS DISTINCT FROM` on jsonb | Q120, Q128 | structural (key-order-insensitive), null-safe equality |
| `x::text` on jsonb, `~`, `ILIKE` | Q187, list `contains`, list sort | the **canonical text rendering** of jsonb (`["a", "b"]`, `{"k": 1}` — space after `,` and `:`) is observable: `base-filters.test.ts` matches a text filter against `["a", "b"]`, and list columns sort by that text |
| `(x->>'k')::boolean` | F7, Q109 | text → boolean cast |

Storage semantics that leak into behaviour:

- jsonb does not keep **object key order** (it stores keys sorted by length, then bytes) nor duplicate keys.
  Documents and cells come back in that order, and `base-cells.test.ts:744` pins a consequence: a file cell
  re-sent unchanged is seen as changed because the app compares serialised JSON and the stored key order differs.
- Numbers are arbitrary-precision `numeric` inside jsonb (`1e21`, `0.1` are exact); the Rust server normalises
  them back to JavaScript number formatting when reading (`db.rs:389-396`).
- jsonb rejects `\u0000` and unpaired surrogates (§3.4).
- No JSON path queries, no GIN index on any jsonb column, no JSON schema validation in the database.

### 4.7 Arrays

`text[]`: `workspace.email_domains`. `uuid[]`: `pages.contributor_ids`, `page_versions.contributor_ids`,
`invitations.group_ids`. Operations: `col = ANY($1::uuid[])` / `ANY($1::text[])` as the universal "IN list"
(some 30 call sites, lists up to 2000 ids), `NOT (col = ANY(…))`, `unnest`, `||` concatenation,
`ARRAY(SELECT DISTINCT unnest(…))` set-union, `ARRAY[$1]::uuid[]`, `array_position`, `::text[]` casts on output,
whole-array replacement with `COALESCE($1::text[], col)`, `text[]` columns inside `jsonb_to_recordset`.

### 4.8 Cascade deletes the app relies on

The app deletes only the top object and counts on the graph of §1.28:

- **Delete a space** (Q71): pages → their versions, comments (→ replies, comment notifications), shares,
  favourites, watchers, labels links, link graph, synced blocks, notifications, base properties/rows/views; plus
  space memberships, space favourites/watchers; `workspace.default_space_id` → NULL. Files are deleted explicitly
  first because their FK is only `SET NULL` and the storage keys must be returned.
- **Delete a page subtree** (Q108): the statement deletes every id of the subtree itself (the self-referencing
  cascade would do the same) and files explicitly; everything else per page cascades.
- **Delete a user** (Q34): sessions, tokens, group and space memberships, favourites, watchers, notifications
  cascade; authorship columns become NULL (pages, comments, files … survive; `contributor_ids` keep the dangling id).
- **Delete a group** (Q52): its memberships and its space grants cascade — an implicit permission change, which
  is why the route re-checks open sockets afterwards.
- **Delete a comment** (Q137): replies and their notifications. **Delete a label** (Q159): its page links.

### 4.9 Collation and case-folding

- `position` (pages, base rows, base views, base properties) is `COLLATE "C"`: plain byte order over
  `[0-9A-Za-z]`, which the fractional-index algorithm requires (`0-9 < A-Z < a-z`). The keyset cursor forces the
  same collation on its parameter (`$1::text COLLATE "C"`).
- Everything else uses the **database's default collation and ctype**, which is a deployment property:
  - unique indexes on `lower(email)`, `lower(name)`, `lower(slug)`, `lower(btrim(name))`;
  - `ORDER BY lower(name)` / `lower(coalesce(title,''))` for members, groups, spaces, backlinks;
  - `ILIKE` (document fallback search, base text filters) and `lower(text) LIKE`;
  - base text filters `gt/gte/lt/lte` and text sorts compare in that collation — the API tests note "text
    comparisons follow the database's collation; the cases here are chosen so that every collation agrees"
    (`base-filters.test.ts:12`, `base-sorts.test.ts:12`);
  - `labels.name` uniqueness and `l.name LIKE` are case-sensitive (the app lower-cases).
- The app does its own Unicode lower-casing/NFKC on search needles and emails before they reach SQL; the two
  servers agree with each other up to Unicode 16 (`src/server-rs/README.md`), but neither is guaranteed to agree
  with the database's `lower()`.
- `btrim()` trims spaces only; the app's trim is JavaScript's (all Unicode whitespace).

### 4.10 Things in SQL that are really application logic

Worth knowing because a typed API has to host them somewhere: the permission model (F3/F4 inside 17 query shapes),
`sharing_enabled` (F7), snapshot throttling (Q120), notification eligibility and throttling (Q162, Q156), trash
semantics by timestamp equality (Q89, Q106, Q107), the five `cell_*`/patch functions, URL building in F5/F6
(`'/api/images/' || icon`).

---

## 5. Migrations and lifecycle

- **Migrations** are five plain SQL files embedded in both binaries at build time (`db.rs:708-714` uses
  `include_str!` on the TS tree's files — one source of truth). At start: Q1 → Q2 → for each unapplied file, T1.
  There are no down-migrations and no checksums; a file is identified by name. DDL is transactional (a failed
  file leaves nothing behind). The README requires migrations to stay compatible with the previous code version
  because two versions run on one database during a rolling deploy ("add to the schema, and only remove what it
  no longer uses").
- **Startup order** (`main.rs:69-98`): validate SSO config → connect pool → migrate → try for the lease
  (Q190) → start the 60 s socket-session check → listen. A database that cannot be reached = log and exit 1.
- **Connection**: `DATABASE_URL` (`postgres://` or `postgresql://`, TCP or `?host=/socket/dir` Unix socket);
  pool size `DATABASE_POOL_MAX`, **default 5**. No TLS in the Rust server. No per-connection setup statements.
- **Multi-process assumptions**: 1–3 copies of the server (either implementation, mixed versions) share one
  database and one file store. Exactly one leads realtime (§4.1); all serve the HTTP API. Everything shared is in
  the database, sessions included. Per-process memory only: rate-limit counters, the outgoing-mail queue (there
  is no outbox table; unsent mail is lost with the process), SSO re-check back-off, the socket registry and the
  open collaborative documents with their unsaved steps. Each copy runs its own housekeeping.
- **Periodic jobs** (per process; no job table, no scheduler in the database):

  | period | job | statements |
  |---|---|---|
  | 60 s after start, then hourly | purge trash older than `settings.trashRetentionDays` (default 30) with files; purge expired sessions and tokens (`main.rs:35-47`) | Q8, Q108, Q44, Q48 |
  | every 3 s (leader) / 1 s (others) | renew or try the lease | Q190 |
  | every 60 s (leader) | check that the sessions of all open sockets are still valid | Q36 (+ Q41/Q39 for SSO re-checks) |
  | 2 s after the last edit, at most 10 s after the first unsaved edit, and when the last editor leaves | save an open document | T16 + follow-ups |
  | per session, at most once a minute | slide session expiry | Q38 |

- **Health**: `GET /api/health` runs `SELECT 1`.
- **Tests as a constraint on the database interface**: the black-box API suite (`src/api-tests`, shared by both
  servers) opens its own connection and issues **about 420 raw SQL statements** to arrange and inspect state:
  resetting with `DROP SCHEMA public CASCADE`, back-dating `created_at`/`updated_at`/`deleted_at`/`expires_at`,
  writing `content`/`cells` directly, reading any table, creating **triggers** that sleep or raise to simulate
  slow or failing writes, holding `FOR UPDATE`/`LOCK TABLE` from outside, and querying `pg_stat_activity`.
  A replacement needs an equivalent out-of-band admin/inspection channel (and fault injection) for this suite
  to survive.

---

## 6. Scale hints

Design envelope: a self-hosted wiki for a team, "built to run in a small container"; the CI runs the whole
server under a **128 MiB** memory limit, and the default pool is 5 connections. Workspaces are single-tenant
(one `workspace` row).

Row and value sizes

| table | growth | row size |
|---|---|---|
| `pages` | one per page; never shrinks except trash purge | `content` jsonb: typically KBs, capped at 2 000 000 positions / 8 MiB request; `text_content` similar order; `search` tsvector up to ~1 MB |
| `page_versions` | **largest by volume**: a full copy of the document at most every 5 min of active editing per page (every 60 s in a page's first 5 minutes); **never pruned** | same as `content` |
| `base_rows` | one per table row; the code is written for bases of thousands to tens of thousands of rows (API pages ≤ 500 rows, public shares 1000, Markdown/HTML export 10 000, CSV export streams all rows in 500-row keyset batches) | `cells` jsonb, text cells up to 100 000 UTF-16 units each, request ≤ 4 MiB |
| `notifications` | one per recipient per event; never deleted | small |
| `sessions` | one per sign-in, 30-day sliding expiry, purged hourly | small |
| `page_links`, `synced_blocks`, `watchers`, `comments`, `files` | proportional to pages | small, except `synced_blocks.content` and `comments.body` (small documents) |
| `users`, `groups`, `spaces`, memberships, `labels`, `invitations` | tens to thousands | small |
| `workspace`, `realtime_leader` | exactly one row | |

Hot paths (database round trips)

- **Every authenticated HTTP request**: Q35 (join by unique `token_hash`), plus Q38 at most once a minute per
  session, before the handler runs.
- **Opening a page** (`GET /pages/:page`): Q35 → Q80 (with content, two correlated subqueries for role and
  children) → Q64 (space) → Q15 (people) → Q98 (recursive breadcrumbs): 5 queries. The client then typically
  asks for the tree children (Q65 + Q87 per expanded node), comments (Q80 + Q131), labels, share state (Q80 +
  Q144 + Q102), watch state, favourites, unread count (Q165).
- **Typing in the editor: zero database work per keystroke.** Steps are validated and applied in memory by the
  lease holder and broadcast over WebSockets. Per *join*: 2× Q80 (read and write permission) and, for the first
  editor of a page, Q82 (whole document; plus Q83 when an idle in-memory session is taken over). Per *save*
  (debounced 2 s, max 10 s): T16 = 5–7 statements in one transaction, including **reading the previous document back under lock and writing the whole new document**,
  re-deriving the tsvector, deleting and re-inserting the link graph and synced blocks, and the snapshot check;
  then 2–5 autocommit statements for watchers and notifications. A busy page therefore costs roughly one
  full-document read + write every 2–10 s regardless of typing speed. `POST /pages/:id/content` (imports) does
  the same synchronously.
- **Base grid**: each query = Q35 → Q80 → Q167 → Q188/Q189 (limit+1 rows of jsonb) → Q15 + Q92 for references.
  Each cell edit = Q35 → Q80 → Q167 → T21 (lock, update with `apply_cell_patch`, return row). Sorted views page
  by OFFSET with computed sort keys (no index can serve them): cost grows with the page number and the base size.
  Filters and sorts on `cells` are always evaluated row by row over all rows of the base (only
  `base_rows_order_idx (page_id, position, id)` exists).
- **Schema changes on a base** (T22–T24) and **duplicate** (T17) are the heavy writers: whole-table rewrites in
  one transaction; duplicate loads up to 2000 full documents into memory and inserts them as one JSON parameter.
- **Permission subqueries** (F3/F4) and the `has_children` EXISTS (F1) are evaluated in nearly every read; they
  rely on `space_members_user_idx`, `group_members_user_idx`, the partial unique membership indexes and
  `pages_parent_idx`.
- **Search**: Q118 uses the GIN index for the full-text branch, but the `OR lower(coalesce(title,'')) LIKE '%…%'`
  branch and the Q119 fallback are scans over all pages in scope (`text_content` included for Q119).
- **Background**: one tiny upsert every 1–3 s per process (Q190) — the steadiest write load on an idle system;
  hourly purge (recursive delete).
- After membership or tree changes the leader re-checks every affected open editor: 2× Q80 per open
  (socket, page) pair, plus Q66 per affected user.

Unindexed predicates that work today only because the tables are small: `files.storage_key` (Q143),
`files.space_id` (Q71), `sessions.expires_at`/`user_tokens.expires_at` (Q44, Q48), `comments.parent_id` (Q133),
`users.role` (Q24), `notifications (user_id, read_at IS NULL)` (Q165, served by the `(user_id, created_at)` prefix),
all `lower(x) LIKE '%…%'` filters.

---

## 7. Hardest things to replicate without SQL (ranked)

1. **The base row engine (§2.25).** Arbitrary user-built filter trees and multi-key sorts over schemaless JSON
   cells, with five coercion functions whose edge cases are PostgreSQL's own: `text::numeric`, the full
   `text::timestamptz` input grammar (the fixtures rely on `"March 5, 2026"` parsing and `"2026"` /
   `"2026-02-30"` not parsing), exact-decimal number comparison, collation-dependent text comparison, `ILIKE`,
   membership in JSON arrays, substring match on jsonb's canonical text, NULLS LAST in both directions,
   `array_position` ordering, and a sort key fetched from another table per row — combined with keyset pagination
   when unsorted, OFFSET when sorted, and the same scan reused inside write transactions.
2. **Full-text search (§4.2).** A weighted, stored, automatically maintained inverted index with prefix AND
   queries, `ts_rank` ordering mixed with an exact-title boost, an OR'd substring branch, a scope predicate that
   may itself be a recursive subtree, a substring fallback, and server-side snippet extraction; the tokenizer is
   PostgreSQL's default parser.
3. **Atomic recursive tree operations (§2.12).** Trash/restore keyed on identical transaction timestamps,
   permanent delete returning two kinds of results (file keys and page ids) from one statement, cross-space move
   with per-row conditional rewrites and conditional share removal, ancestor walks that stop at a boundary for
   public-share checks (on every public request), cycle detection, breadth-first ordered subtree reads, and a
   subtree used as a search scope.
4. **The save transaction (T16) and its invariants.** Row lock + optimistic version check + whole-document
   replace + rebuild of two derived tables (with "update only if changed" upsert semantics) + a time-windowed,
   content-distinct snapshot insert, all atomic, with blocking (not failing) lock semantics that the test-suite
   exercises from a second connection.
5. **Multi-process coordination through the database.** A lease as a conditional upsert judged by the database
   clock, three named transaction-scoped mutexes with no backing rows (advisory locks), migrations that tolerate
   concurrent starters, sessions and versions visible across processes immediately. An embedded single-process
   store cannot offer this without a server or a shared-file locking protocol.
6. **Constraint-driven control flow.** Expression unique indexes (`lower(email)`, `lower(btrim(name))`), partial
   unique indexes used as upsert targets (`(space_id, user_id) WHERE user_id IS NOT NULL`), upserts that update
   only some columns or only when content differs, typed errors the app branches on (`23505` → 409/400/retry,
   `40P01` deadlock detection → retry), and conditional `UPDATE/DELETE … RETURNING` as compare-and-swap,
   including "return the value before the update" (Q12, Q31, Q142).
7. **Referential actions.** 50 foreign keys with CASCADE or SET NULL, two self-references (`pages.parent_id`,
   `comments.parent_id`), the delete of a space or user fanning out through a dozen tables — the app issues one
   DELETE and trusts the rest.
8. **The permission predicate everywhere.** "max role over direct and group grants" as a correlated scalar or
   semi-join in 17 query shapes (including the one behind every page request), inside LEFT JOIN conditions (so unreadable rows degrade to NULL rather
   than disappear: Q130, Q164), inside an INSERT … SELECT (Q162), and around derived tables (Q64).
9. **jsonb fidelity.** Structural equality, shallow merge / strip-nulls / key removal, set-returning expansion of
   JSON parameters into typed rows (the bulk-write mechanism for up to 2000 rows), JSON construction in results,
   plus incidental behaviours the tests pin: key reordering, canonical text form, rejection of NUL and lone
   surrogates surfacing as 500s.
10. **Test-suite coupling.** ~420 raw SQL statements, triggers for fault injection, external row/table locks and
    `pg_stat_activity` in the shared API suite (§5): replacing the engine means replacing that back door too.
