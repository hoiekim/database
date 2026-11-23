# `mail` — exhaustive inventory of PostgreSQL usage

Source: `the mail repository`. Primary = Rust server (`src/server-rs/src`, paths below are relative to it unless prefixed).
Cross-check = Bun/TS server (`src/server/lib`, prefixed `TS:`). Both run against **one shared database** and are
required to emit the same SQL "text for text" (golden tests pin this), so unless a line says "differs", the TS server
issues the identical statement.

Conventions used by both servers that shape every query:

- Statements are **unprepared**, one round trip each, on a pool of 10 connections (`postgres/pool.rs:443`, `TS: postgres/pool.ts:218`).
- **Every parameter is bound as untyped text** and typed by the statement (`postgres/pool.rs:54-81`); arrays are sent as
  Postgres array literals `{"a","b"}`, JSON as JSON text. SQL therefore carries explicit casts where inference is not
  enough (`$3::int`, `$2::jsonb`, `$2::uuid[]`, `$2::text[]`, `$3::timestamptz`, `COUNT(*)::int`).
- Rows are decoded dynamically (`postgres/pool.rs:239-271`): `int8`/`numeric` -> JSON number, `timestamptz` -> ISO-8601
  string truncated to **milliseconds** (`...Z`), `uuid` -> lowercase hyphenated string, `jsonb` -> parsed JSON,
  `bytea` -> hex, `tsvector` -> its text form, arrays -> JSON arrays.
- `row_count` (rows returned or affected) and `RETURNING` are read by the app on most writes (see 3.6).
- No ORM. Three layers build SQL: a tiny generic `Table` wrapper (`postgres/repositories/mails/table.rs`), pure
  "query builder" functions per feature, and a few literal statements.

---

## 1. Schema

11 tables: 10 in `src/server-rs/schema.json` (generated from `TS: postgres/models/*.ts` by `scripts/schema-snapshot.ts`)
plus `schema_meta` created ad hoc (`postgres/migration.rs:273`). No enum types, no sequences/serial/identity, no
CHECK constraints, no generated columns, no `CREATE EXTENSION` (`gen_random_uuid()` is core in PG >= 13; compose runs
`postgres:16-alpine`). One PL/pgSQL trigger function with two triggers. "srv-gen" below = value produced by the server
when the app omits the column.

### 1.1 `users`
| column | type | null | default | notes |
|---|---|---|---|---|
| user_id | UUID | NOT NULL | `gen_random_uuid()` | **PK**. srv-gen for SSO-created rows and first admin seed; app-generated UUIDv4 for pending sign-ups (crate-root `users.rs:109`) |
| username | VARCHAR(255) | NOT NULL | – | **UNIQUE** |
| password | VARCHAR(255) | NULL | – | bcrypt hash; NULL = unfinished sign-up |
| password_version | INTEGER | NULL | 1 | credential generation ("revocation fence"); NULL read as 1 |
| email | VARCHAR(255) | NULL | – | not unique |
| expiry | TIMESTAMPTZ | NULL | – | sign-up/reset token expiry |
| token | VARCHAR(255) | NULL | – | |
| updated | TIMESTAMPTZ | NULL | `CURRENT_TIMESTAMP` | every app write sets `updated = CURRENT_TIMESTAMP` explicitly |
| is_deleted | BOOLEAN | NULL | FALSE | soft-delete flag; **never set TRUE by current code** but filtered in every read |
| imap_uid_validity | BIGINT | NULL | – | lazily set to unix seconds on first IMAP use |
| auth_sub | TEXT | NULL | – | **UNIQUE** (many NULLs allowed) – OIDC subject |

Indexes: PK(user_id), UNIQUE(username), UNIQUE(auth_sub), `idx_users_email` btree(email).

### 1.2 `sessions`
| column | type | null | default |
|---|---|---|---|
| session_id | VARCHAR(255) | NOT NULL | – (**PK**, app-generated random id) |
| session_user_id | UUID | NOT NULL | – (**no FK** to users) |
| session_username | VARCHAR(255) | NOT NULL | – |
| session_email | VARCHAR(255) | NOT NULL | – |
| session_is_read_only | BOOLEAN | NULL | – |
| session_authenticated_as | VARCHAR(255) | NULL | – |
| session_password_version | INTEGER | NULL | – |
| auth_sid | TEXT | NULL | – |
| auth_refresh_token | TEXT | NULL | – |
| auth_recheck_at | TIMESTAMPTZ | NULL | – |
| cookie_original_max_age | BIGINT | NULL | – |
| cookie_max_age | BIGINT | NULL | – |
| cookie_signed | BOOLEAN | NULL | – |
| cookie_expires | TIMESTAMPTZ | NULL | – |
| cookie_http_only | BOOLEAN | NULL | – |
| cookie_path | TEXT | NULL | – |
| cookie_domain | TEXT | NULL | – |
| cookie_secure | VARCHAR(10) | NULL | – (JSON text, e.g. `true`, `"auto"`) |
| cookie_same_site | VARCHAR(20) | NULL | – (JSON text, e.g. `"strict"`) |
| updated | TIMESTAMPTZ | NULL | `CURRENT_TIMESTAMP` |

Indexes: PK, `idx_sessions_session_user_id`, `idx_sessions_cookie_expires`, `idx_sessions_auth_sid` (all btree).

### 1.3 `mailboxes` (user-created IMAP folders only; INBOX/Sent/Drafts/Junk/Starred/Trash and per-account boxes are synthetic)
| column | type | null | default | notes |
|---|---|---|---|---|
| mailbox_id | UUID | NOT NULL | `gen_random_uuid()` | **PK**, always srv-gen |
| user_id | UUID | NOT NULL | – | **FK -> users(user_id) ON DELETE CASCADE** |
| name | VARCHAR(255) | NOT NULL | – | **no unique constraint**; app enforces case-insensitive uniqueness per user by probe (racy); app caps at 255 *bytes* |
| address | VARCHAR(255) | NULL | – | |
| parent_id | UUID | NULL | – | **FK -> mailboxes(mailbox_id) ON DELETE CASCADE** (always NULL today) |
| uid_validity | INTEGER | NOT NULL | 1 | bumped (+1, app-computed) on RENAME; **not what IMAP serves** (that is `users.imap_uid_validity`) |
| uid_next | INTEGER | NOT NULL | 1 | written as 1 on create, never read/updated (UIDs come from `mail_uid_counters`) |
| subscribed | BOOLEAN | NOT NULL | TRUE | |
| special_use | VARCHAR(50) | NULL | – | |
| created | TIMESTAMPTZ | NULL | `CURRENT_TIMESTAMP` | srv-gen; table has **no `updated`** column |

Indexes: PK, `idx_mailboxes_user_id`, `idx_mailboxes_name`, `idx_mailboxes_address`, `idx_mailboxes_parent_id` (btree).

### 1.4 `mails` (the big table; one row per (user, Message-ID), including COPY/MOVE clones and soft-deleted rows)
| column | type | null | default | notes |
|---|---|---|---|---|
| mail_id | UUID | NOT NULL | `gen_random_uuid()` | **PK**; in practice always app-generated UUIDv4 (`core.rs:314`) |
| user_id | UUID | NOT NULL | – | **FK -> users ON DELETE CASCADE** |
| message_id | VARCHAR(512) | NOT NULL | – | **UNIQUE (user_id, message_id)** |
| subject | TEXT | NOT NULL | '' | |
| date | TIMESTAMPTZ | NOT NULL | `CURRENT_TIMESTAMP` | app always supplies (header date or app clock); microsecond precision matters for cursors |
| html | TEXT | NOT NULL | '' | decoded HTML body (can be many MB) |
| text | TEXT | NOT NULL | '' | decoded plain-text body (can be many MB) |
| from_address | JSONB | NULL | – | array of `{address, name}` |
| from_text | TEXT | NULL | – | header as received |
| to_address / to_text | JSONB / TEXT | NULL | – | |
| cc_address / cc_text | JSONB / TEXT | NULL | – | |
| bcc_address / bcc_text | JSONB / TEXT | NULL | – | |
| reply_to_address / reply_to_text | JSONB / TEXT | NULL | – | |
| envelope_from | JSONB | NULL | – | array of `{address[,name]}` |
| envelope_to | JSONB | NULL | – | array; merged (append unseen addresses) on duplicate delivery |
| attachments | JSONB | NULL | – | array of `{filename, contentType, content:{data:<file id>}, size}`; bytes live on disk |
| read, saved, sent, deleted, draft, answered, expunged | BOOLEAN | NOT NULL | FALSE | IMAP flags `\Seen \Flagged – \Deleted \Draft \Answered`; `expunged` = soft delete |
| insight | JSONB | NULL | – | opaque object |
| uid_domain | INTEGER | NOT NULL | 0 | per-(user, sent) "domain-wide" IMAP UID |
| modseq | BIGINT | NOT NULL | 1 | CONDSTORE mod-sequence |
| spam_score | INTEGER | NOT NULL | 0 | |
| spam_reasons | JSONB | NULL | – | array of strings |
| is_spam | BOOLEAN | NOT NULL | FALSE | |
| rfc822_size | BIGINT | NULL | – | cached derived size |
| text_line_count, html_line_count | INTEGER | NULL | – | declared, never written/read by queries |
| in_reply_to | VARCHAR(512) | NULL | – | |
| reference_ids | JSONB | NULL | – | array of Message-ID strings |
| updated | TIMESTAMPTZ | NULL | `CURRENT_TIMESTAMP` | set from **DB clock** on semantic mutations; drives delta sync |
| search_vector | TSVECTOR | NULL | – | maintained by trigger (below) |

Constraints: PK(mail_id); `UNIQUE (user_id, message_id)` (named `mails_user_message_unique` on DBs migrated by
`migrations/001_unique_user_message_id.sql`); FK user_id.

Trigger (schema.json `searchVectorDdl`, `TS: postgres/search-vector.ts`):
```sql
CREATE OR REPLACE FUNCTION mails_search_vector_trigger() RETURNS trigger AS $$
BEGIN
  NEW.search_vector := to_tsvector('english',
    coalesce(replace(replace(NEW.subject,   '<', ' '), '>', ' '), '') || ' ' ||
    coalesce(NEW.text, '') || ' ' ||
    coalesce(replace(replace(NEW.from_text, '<', ' '), '>', ' '), '') || ' ' ||
    coalesce(replace(replace(NEW.to_text,   '<', ' '), '>', ' '), ''));
  RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE TRIGGER mails_search_insert BEFORE INSERT ON mails FOR EACH ROW EXECUTE FUNCTION mails_search_vector_trigger();
CREATE TRIGGER mails_search_update BEFORE UPDATE OF subject, text, from_text, to_text ON mails
  FOR EACH ROW EXECUTE FUNCTION mails_search_vector_trigger();
```
(`UPDATE OF` fires when a listed column is in the SET list, changed or not. No app UPDATE currently touches those four
columns, so in practice the vector is computed once at INSERT; the reindex statement in 5.3 repairs old rows.)

Indexes on `mails` (all `CREATE INDEX CONCURRENTLY IF NOT EXISTS`, built in boot maintenance):
- btree single-column: `idx_mails_user_id`(user_id), `idx_mails_date`(date), `idx_mails_sent`, `idx_mails_read`,
  `idx_mails_saved`, `idx_mails_uid_domain`, `idx_mails_modseq`, `idx_mails_is_spam`, `idx_mails_draft`,
  `idx_mails_expunged`, `idx_mails_in_reply_to`.
- GIN `jsonb_path_ops` (for `@>`): `idx_mails_to_address_gin`, `idx_mails_cc_address_gin`, `idx_mails_bcc_address_gin`,
  `idx_mails_envelope_to_gin`, `idx_mails_from_address_gin`.
- GIN default `jsonb_ops` (for `?|`): `idx_mails_reference_ids_gin`(reference_ids).
- GIN tsvector: `idx_mails_search`(search_vector).
- Partial expression btree:
  ```sql
  CREATE INDEX idx_mails_thread_subject_key ON mails (user_id,
    (md5(lower(btrim(regexp_replace(regexp_replace(subject,
      '^\s*((re|fwd?|aw|wg|sv|vs|antw|tr|rv)\s*(\[\d+\])?\s*[:：]\s*)+', '', 'i'), '\s+', ' ', 'g'))))))
    WHERE in_reply_to IS NULL AND reference_ids IS NULL;
  ```
- Implicit: PK(mail_id), UNIQUE(user_id, message_id).
- Note: there is **no** composite index such as (user_id, sent, uid_domain) or (user_id, date); no index on `updated`;
  no trigram index (IMAP SEARCH `ILIKE` is a scan).

### 1.5 `push_subscriptions`
push_subscription_id UUID **PK** default `gen_random_uuid()` (app supplies UUIDv4); user_id UUID NOT NULL **FK -> users ON
DELETE CASCADE**; endpoint TEXT NOT NULL; keys_p256dh TEXT NOT NULL; keys_auth TEXT NOT NULL; last_notified
TIMESTAMPTZ NULL; updated TIMESTAMPTZ NULL default `CURRENT_TIMESTAMP`. Index `idx_push_subscriptions_user_id`.

### 1.6 `spam_allowlist`
allowlist_id UUID **PK** default `gen_random_uuid()` (srv-gen); user_id UUID NOT NULL **FK -> users ON DELETE CASCADE**;
pattern TEXT NOT NULL (stored lowercased by app; <= 320 bytes, <= 1000 rows/user enforced by app/SQL); created_at
TIMESTAMPTZ NOT NULL default `CURRENT_TIMESTAMP` (srv-gen). **UNIQUE(user_id, pattern)**. Index
`idx_spam_allowlist_user_id`. No index on `LOWER(pattern)`.

### 1.7 `spam_training`
training_id UUID **PK** default `gen_random_uuid()` (srv-gen); user_id UUID NOT NULL **FK -> users ON DELETE CASCADE**;
word VARCHAR(100) NOT NULL (over-length word makes the whole statement fail – relied on, `spam_training.rs:145`);
spam_count INTEGER NOT NULL default 0; ham_count INTEGER NOT NULL default 0. **UNIQUE(user_id, word)**. Sentinel row
`word = '__total__'` holds per-class document totals. Index `idx_spam_training_user_id`.

### 1.8 `mail_uid_counters` (the UIDNEXT / MODSEQ authority)
user_id UUID NOT NULL (**no FK**); uid_kind VARCHAR(16) NOT NULL (`domain` | `account` | `mailbox` | `modseq`);
uid_scope TEXT NOT NULL (`''`, an email address, or `Starred`/`Trash`); sent BOOLEAN NOT NULL; last_uid BIGINT NOT NULL.
**PK (user_id, uid_kind, uid_scope, sent)**. No other index.

### 1.9 `mail_mailbox_uid` (membership + per-mailbox UID for "mapped" boxes; scope markers for INBOX)
user_id UUID NOT NULL (no FK); mailbox TEXT NOT NULL (full IMAP path, e.g. `INBOX/accounts/sales`,
`Sent Messages/accounts/sales`, `Archive`, `Starred`, `Trash`, `INBOX`); mail_id UUID NOT NULL **FK -> mails(mail_id) ON
DELETE CASCADE**; uid BIGINT NOT NULL. **PK (user_id, mailbox, mail_id)**; **UNIQUE (user_id, mailbox, uid)**.
Index `idx_mail_mailbox_uid_mail_id`(mail_id).

### 1.10 `mailgun_events`
event_id TEXT **PK**; event TEXT NOT NULL; message_id, recipient, severity, reason TEXT NULL; occurred_at TIMESTAMPTZ
NOT NULL; received_at TIMESTAMPTZ NOT NULL default `CURRENT_TIMESTAMP`; raw JSONB NOT NULL (<= 16 KiB, app-bounded);
updated TIMESTAMPTZ NOT NULL default `CURRENT_TIMESTAMP`. Indexes: `idx_mailgun_events_message_id`,
`idx_mailgun_events_event`, `idx_mailgun_events_occurred_at`. Append-only; never read by the app.

### 1.11 `schema_meta`
key TEXT **PK**; value TEXT NOT NULL. Two rows: `schema_hash`, `maintenance_hash`.

### 1.12 Cascade graph
users -> mailboxes, mails, push_subscriptions, spam_allowlist, spam_training (CASCADE); mails -> mail_mailbox_uid
(CASCADE); mailboxes.parent_id -> mailboxes (CASCADE). `sessions`, `mail_uid_counters`, and `mail_mailbox_uid.user_id`
have **no FK** (sessions are cleaned by explicit deletes; counter rows are orphaned by user deletion).

### 1.13 Legacy one-off SQL (not run by the servers)
- `migrations/001_unique_user_message_id.sql`: `CREATE TEMP TABLE … AS SELECT DISTINCT ON (user_id, message_id) mail_id
  FROM mails ORDER BY user_id, message_id, mail_id`; `DELETE … WHERE mail_id NOT IN (SELECT …)`; `ALTER TABLE … ADD
  CONSTRAINT … UNIQUE`.
- `scripts/725-backfill-starred-trash-pivots.sql`: `BEGIN … COMMIT`, CTEs, `ROW_NUMBER() OVER (PARTITION BY m.user_id ORDER
  BY m.date ASC, m.mail_id ASC)`, `LEFT JOIN`, `NOT EXISTS`, `INSERT … SELECT … ON CONFLICT DO NOTHING`, counter seed with
  `GREATEST(mail_uid_counters.last_uid, EXCLUDED.last_uid)`. This is the **only** place window functions / DISTINCT ON /
  GREATEST / LEFT JOIN appear; the running app uses none of them.
- `scripts/migrate-es-to-pg.ts` (historic Elasticsearch -> PG import): per-row INSERTs into users / sessions (with
  `ON CONFLICT (pk) DO UPDATE`), mails and push_subscriptions. `src/server/backfill.ts` is a deprecated stub.

---

## 2. Query catalogue

Numbered `Qn`. `{…}` = dynamic fragment; the set of values it can take is listed. Membership fragment used by all
IMAP statements (`postgres/repositories/mails/views.rs:91-124`), `p` = column prefix (`""`, `m.` or `z.`):

| mailbox argument | `membership_expression` | `membership_condition` (suffix) |
|---|---|---|
| `None` + sent=false (INBOX) or `INBOX/accounts/<x>` (sent=false) | `p.is_spam = FALSE AND p.draft = FALSE` | ` AND p.is_spam = FALSE AND p.draft = FALSE` |
| `Drafts` (any case) | `p.draft = TRUE` | ` AND p.draft = TRUE` |
| `Junk` (any case) | `p.is_spam = TRUE` | ` AND p.is_spam = TRUE` |
| `None` + sent=true (Sent Messages), `Sent Messages/accounts/<x>`, user boxes, `Starred`, `Trash` | `TRUE` | `` (empty) |

Two IMAP "UID spaces" (`views.rs:67`): **domain views** (mailbox `None`, `Drafts`, `Junk`) enumerate `mails.uid_domain`
with a predicate; **mapped boxes** (everything else) join `mail_mailbox_uid x` on `(user_id, mailbox = $3, mail_id)` and
enumerate `x.uid`. `sent` param = true only for `Sent Messages` / `Sent Messages/accounts/*`.

### 2.1 System / catalog / session statements
- **Q1** `SELECT value FROM schema_meta WHERE key = $1` — `postgres/migration.rs:241` (any error => "not at target").
- **Q2** `CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)` — `migration.rs:273`.
- **Q3** `INSERT INTO schema_meta (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value` — `migration.rs:282`.
- **Q4** `SELECT column_name, data_type, udt_name, is_nullable, column_default FROM information_schema.columns WHERE table_name = $1 AND table_schema = 'public' ORDER BY ordinal_position` — `migration.rs:128`.
- **Q5** `ALTER TABLE {t} ADD COLUMN IF NOT EXISTS {col} {definition}` (PRIMARY KEY stripped; NOT NULL without DEFAULT gets an inferred default) — `migration.rs:96-115,149`.
- **Q6** `SELECT pg_advisory_lock($1)` / `SELECT pg_advisory_unlock($1)` key 6908002 — `migration.rs:178,213`.
- **Q7** `BEGIN` / `COMMIT` / `ROLLBACK` — `migration.rs:179,199,210` (the only explicit transaction).
- **Q8** `SELECT pg_try_advisory_lock($1) AS locked` key 6908003; unlock at end — `postgres/maintenance.rs:186,243`.
- **Q9** `SELECT pg_backend_pid() AS pid`; from another connection `SELECT pg_cancel_backend($1)` — `maintenance.rs:206,218`.
- **Q10** `SET statement_timeout = 600000` (session GUC) — `maintenance.rs:92`.
- **Q11** invalid-index sweep — `maintenance.rs:55`:
  `SELECT c.relname FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE NOT i.indisvalid AND n.nspname = current_schema() AND c.relname = ANY($1)`
- **Q12** `DROP INDEX CONCURRENTLY IF EXISTS {name}` — `maintenance.rs:77`.
- **Q13** 34 x `CREATE INDEX CONCURRENTLY IF NOT EXISTS …` (section 1) — `maintenance.rs:122` over `schema.json.indexes`.
- **Q14** search-vector reindex, re-issued until it reports 0 rows ("drain") — `schema.json.statements[0]`, `maintenance.rs:121-147`:
  `UPDATE mails SET search_vector = to_tsvector('english', <expr>) WHERE mail_id IN (SELECT mail_id FROM mails WHERE search_vector IS DISTINCT FROM to_tsvector('english', <expr>) LIMIT 1000)`
- **Q15** 10 x `CREATE TABLE IF NOT EXISTS …`, then trigger DDL (`CREATE OR REPLACE FUNCTION`, `DROP TRIGGER IF EXISTS` x2, `CREATE TRIGGER` x2) — `postgres/initialize.rs:54,59`.
- **Q16** `SELECT 1` — health probe, `http/routes/health.rs:72`.

### 2.2 `users`
In this subsection `R:` = `postgres/repositories/users.rs` and `U:` = the crate-root `users.rs` (account flows).
- **Q17** point lookup, equality on 1..3 columns + soft-delete predicate — `R:82-98`:
  `SELECT * FROM users WHERE {c1} = $1 [AND {c2} = $2 [AND {c3} = $3]] AND (is_deleted IS NULL OR is_deleted = FALSE) LIMIT $n` (n bound to 1).
  Column sets used: `user_id` (`R:123`; session validation `sessions.rs:212`), `username` (`R:128`; IMAP/SMTP auth and per-command credential check `imap/credential_version.rs:16`, `imap/auth.rs:96`, `smtp/handler.rs:134`), `email` (`U:96,190`), `auth_sub` (`R:133`), and any AND-combination of `user_id, username, email` (`R:102-119`, route `http/routes/users.rs:66`).
- **Q18** upsert by PK — `R:150-190`:
  `INSERT INTO users (updated, username[, password][, user_id][, email][, password_version]) VALUES (CURRENT_TIMESTAMP, $1, …) ON CONFLICT (user_id) DO UPDATE SET {col = EXCLUDED.col for each inserted col except user_id}, updated = CURRENT_TIMESTAMP RETURNING *`
  (admin / admin-ro seeding at boot. When `user_id` is omitted the PK default applies and it is a plain insert; a username collision would then raise 23505.)
- **Q19** atomic credential rotation — `R:204`:
  `UPDATE users SET password = $2, password_version = COALESCE(password_version, 1) + 1, username = $3, token = $4, expiry = $5, updated = CURRENT_TIMESTAMP WHERE user_id = $1 RETURNING password_version`
- **Q20** dynamic update by PK — `R:213-230`:
  `UPDATE users SET updated = CURRENT_TIMESTAMP, {col = $i, …} WHERE user_id = $n RETURNING user_id`.
  Column sets: `{token, expiry}` (values, or both NULL; `U:98,143`), `{imap_uid_validity}` (`R:267`), `{email}` (`R:317`). Returned-row presence = "row exists".
- **Q21** `INSERT INTO users (updated, user_id, username, email, token, expiry) VALUES (CURRENT_TIMESTAMP, $1, $2, $3, $4, $5) RETURNING user_id` — `R:242` (username `user_<8 hex>`; unique violation propagates).
- **Q22** `DELETE FROM users WHERE user_id = $1 RETURNING user_id` — `R:252` (reclaims an expired, never-completed sign-up, `U:146`; cascades).
- **Q23** case-insensitive + NULL tests + `<> ALL(array)` — `R:279`:
  `SELECT * FROM users WHERE LOWER(email) = LOWER($1) AND auth_sub IS NULL AND password IS NOT NULL AND username <> ALL($2::text[]) AND (is_deleted IS NULL OR is_deleted = FALSE) LIMIT 2`
- **Q24** conditional link (compare-and-set) — `R:290`:
  `UPDATE users SET auth_sub = $1, updated = CURRENT_TIMESTAMP WHERE user_id = $2 AND auth_sub IS NULL AND is_deleted IS NOT TRUE RETURNING *`; SQLSTATE 23505 (auth_sub UNIQUE) is caught => `None`.
- **Q25** `INSERT INTO users (updated, username, password, email, auth_sub) VALUES (CURRENT_TIMESTAMP, $1, $2, $3, $4) ON CONFLICT DO NOTHING RETURNING *` — `R:309` (target-less ON CONFLICT: any of PK / username / auth_sub uniqueness => no row).
- **Q26** `UPDATE users SET password_version = COALESCE(password_version, 1) + 1, updated = CURRENT_TIMESTAMP WHERE auth_sub = $1 RETURNING user_id, username, password_version` — `R:327`.
- TS-only leftovers (exported, **not called**): `softDelete` (`UPDATE users SET is_deleted = TRUE, updated = CURRENT_TIMESTAMP WHERE user_id = $1 RETURNING user_id`, `TS: postgres/repositories/users.ts:138`), `updateUser`, `getUserByEmail`. Rust has no soft-delete write.

### 2.3 `sessions`
- **Q27** `SELECT * FROM sessions WHERE session_id = $1 LIMIT $2` — `postgres/repositories/sessions.rs:65`.
- **Q28** upsert — `sessions.rs:84-134`:
  `INSERT INTO sessions (updated, session_id, session_user_id, session_username, session_email, session_is_read_only, session_authenticated_as, session_password_version, auth_sid, auth_refresh_token, auth_recheck_at, cookie_original_max_age[, cookie_max_age][, cookie_signed][, cookie_expires][, cookie_http_only][, cookie_path][, cookie_domain][, cookie_secure][, cookie_same_site]) VALUES (CURRENT_TIMESTAMP, $1…) ON CONFLICT (session_id) DO UPDATE SET {col = EXCLUDED.col …}, updated = CURRENT_TIMESTAMP RETURNING *`
  (omitted optional columns keep their stored values on conflict).
- **Q29** `DELETE FROM sessions WHERE session_id = $1 RETURNING session_id` — `sessions.rs:139`.
- **Q30** `DELETE FROM sessions WHERE cookie_expires IS NOT NULL AND cookie_expires <= $1 RETURNING session_id` ($1 = app-clock ISO time; row count used) — `sessions.rs:151`.
- **Q31** `DELETE FROM sessions WHERE {session_authenticated_as | session_user_id | auth_sid} = $1 RETURNING session_id` (count used) — `sessions.rs:156-168`.

### 2.4 `mailboxes`
- **Q32** `SELECT * FROM mailboxes WHERE user_id = $1` — `postgres/repositories/mailboxes.rs:162` (sorted in the app with ICU root collation = JS `localeCompare`, not by SQL).
- **Q33** `SELECT * FROM mailboxes WHERE user_id = $1 AND LOWER(name) = LOWER($2) LIMIT 1` — `mailboxes.rs:170-176`.
- **Q34** `SELECT * FROM mailboxes WHERE user_id = $1 AND address = $2 LIMIT $3` — `mailboxes.rs:186` (defined; no production caller).
- **Q35** `SELECT COUNT(*)::int AS count FROM mailboxes WHERE user_id = $1` — `mailboxes.rs:226`.
- **Q36** `INSERT INTO mailboxes (user_id, name, address, parent_id, special_use, subscribed, uid_validity, uid_next) VALUES ($1..$8) RETURNING *` — `mailboxes.rs:231` (no `updated` stamp: table has none).
- **Q37** `DELETE FROM mailboxes WHERE mailbox_id = $1 RETURNING mailbox_id` — `mailboxes.rs:266`.
- **Q38** `UPDATE mailboxes SET name = $1, uid_validity = $2 WHERE mailbox_id = $3 RETURNING mailbox_id` ($2 = previously read value + 1) — `mailboxes.rs:293`.
- **Q39** `UPDATE mailboxes SET subscribed = $1 WHERE mailbox_id = $2 RETURNING mailbox_id` — `mailboxes.rs:312`.
  Note: DELETE/RENAME touch only this table; `mail_mailbox_uid.mailbox` rows keyed by the old name are not rewritten or removed.

### 2.5 `push_subscriptions` (`postgres/repositories/push_subscriptions.rs`; every function swallows DB errors)
- **Q40** `INSERT INTO push_subscriptions (updated, push_subscription_id, user_id, endpoint, keys_p256dh, keys_auth, last_notified) VALUES (CURRENT_TIMESTAMP, $1..$6) RETURNING push_subscription_id` — `:36`.
- **Q41** `DELETE FROM push_subscriptions WHERE push_subscription_id = $1 RETURNING push_subscription_id` — `:47`.
- **Q42** `DELETE … WHERE push_subscription_id = $1 AND user_id = $2 RETURNING push_subscription_id` — `:59`.
- **Q43** `DELETE FROM push_subscriptions WHERE updated < $1 RETURNING push_subscription_id` ($1 = app clock − 7 days) — `:72`.
- **Q44** `SELECT * FROM push_subscriptions WHERE user_id IN ($1, …, $n)` — `:85`.
- **Q45** `UPDATE push_subscriptions SET updated = CURRENT_TIMESTAMP WHERE push_subscription_id = $1 AND user_id = $2 RETURNING push_subscription_id` — `:114`.
- **Q46** `UPDATE push_subscriptions SET updated = CURRENT_TIMESTAMP, last_notified = $1 WHERE push_subscription_id = $2 RETURNING push_subscription_id` — `:125`.

### 2.6 `mailgun_events`
- **Q47** `INSERT INTO mailgun_events (updated, event_id, event, message_id, recipient, severity, reason, occurred_at, received_at, raw) VALUES (CURRENT_TIMESTAMP, $1..$9) ON CONFLICT (event_id) DO NOTHING RETURNING *` — `postgres/repositories/mailgun_events.rs:26`.

### 2.7 `spam_allowlist` (`postgres/repositories/spam_allowlists.rs`)
- **Q48** `SELECT * FROM spam_allowlist WHERE user_id = $1 ORDER BY created_at DESC` — `:110`.
- **Q49** `SELECT COUNT(*) AS count FROM spam_allowlist WHERE user_id = $1 AND (LOWER(pattern) = $2 OR LOWER(pattern) = $3)` ($2 = lowercased address, $3 = `*@<domain>`) — `:121`; run for **every received mail**.
- **Q50** capped insert, decided in one statement — `:148`:
  `INSERT INTO spam_allowlist (user_id, pattern) SELECT $1, $2 WHERE (SELECT COUNT(*) FROM spam_allowlist WHERE user_id = $1) < $3 ON CONFLICT (user_id, pattern) DO NOTHING RETURNING *`
- **Q51** `SELECT 1 FROM spam_allowlist WHERE user_id = $1 AND pattern = $2 LIMIT 1` (row_count distinguishes "exists" from "at limit") — `:164`.
- **Q52** `DELETE FROM spam_allowlist WHERE user_id = $1 AND LOWER(pattern) = $2` (row_count) — `:178`.
- **Q53** `DELETE FROM spam_allowlist WHERE user_id = $1 AND allowlist_id = $2` (row_count) — `:190` (no route calls it today).

### 2.8 `spam_training` (`postgres/repositories/spam_training.rs`)
- **Q54** `SELECT * FROM spam_training WHERE user_id = $1` — `:60` (no production caller).
- **Q55** `SELECT spam_count, ham_count FROM spam_training WHERE user_id = $1 AND word = $2` ($2 = `__total__`) — `:64`.
- **Q56** multi-row additive upsert, <= 501 rows (500 distinct words + totals row) — `:95`:
  `INSERT INTO spam_training (user_id, word, spam_count, ham_count) VALUES ($1, $4, $2, $3), ($1, $5, $2, $3), … ON CONFLICT (user_id, word) DO UPDATE SET spam_count = spam_training.spam_count + EXCLUDED.spam_count, ham_count = spam_training.ham_count + EXCLUDED.ham_count`
  (words de-duplicated by the app because one statement cannot upsert the same key twice).
- **Q57** `SELECT * FROM spam_training WHERE user_id = $1 AND word IN ($2, …, $n)` (n <= 501) — `:117`; run per received mail.
- **Q58** `DELETE FROM spam_training WHERE user_id = $1` — `:132` (no production caller).

### 2.9 `mail_uid_counters` (`postgres/repositories/mails/counters.rs`)
- **Q59** atomic reserve-next (UID or MODSEQ) — `:34-40`:
  ```sql
  INSERT INTO mail_uid_counters (user_id, uid_kind, uid_scope, sent, last_uid)
  VALUES ($1, $2, $3, $4, ({seed}))
  ON CONFLICT (user_id, uid_kind, uid_scope, sent)
  DO UPDATE SET last_uid = mail_uid_counters.last_uid + 1
  RETURNING last_uid AS next_uid
  ```
  `{seed}` variants (scalar subqueries evaluated only for the first insert):
  - kind `domain`, scope `''`, sent t/f — `SELECT COALESCE(MAX(uid_domain), 0) + 1 FROM mails WHERE user_id = $1 AND sent = $4` (`:60`, callers `mails/receive.rs:454`, `mails/send.rs:376`, `imap/message_ops.rs:663`).
  - kind `account`, scope = address, sent t/f — `SELECT COALESCE(MAX(uid), 0) + 1 FROM mail_mailbox_uid WHERE user_id = $1 AND mailbox IN ($5, $6)` ($5 = `INBOX/accounts/<local>` or `Sent Messages/accounts/<local>`, $6 = `<local>`) (`:86`).
  - kind `mailbox`, scope = `Starred`|`Trash`, sent false — `SELECT COALESCE(MAX(uid), 0) + 1 FROM mail_mailbox_uid WHERE user_id = $1 AND mailbox = $5` (`:115`).
  - kind `modseq`, scope `''`, sent false — `SELECT COALESCE(MAX(modseq), 0) + 1 FROM mails WHERE user_id = $1` (`:307`).
- **Q60** non-allocating peek (UIDNEXT) — `:51-57`:
  `SELECT COALESCE((SELECT last_uid + 1 FROM mail_uid_counters WHERE user_id = $1 AND uid_kind = $2 AND uid_scope = $3 AND sent = $4), ({seed})) AS uid_next` with the `domain` / `account` / `mailbox` seeds above (`get_uid_next`, `:212`). Errors propagate (never a fabricated value).

### 2.10 `mail_mailbox_uid`
- **Q61** idempotent mapping write that always returns the persisted UID — `counters.rs:271`:
  `INSERT INTO mail_mailbox_uid (user_id, mailbox, mail_id, uid) VALUES ($1, $2, $3, $4) ON CONFLICT (user_id, mailbox, mail_id) DO UPDATE SET uid = mail_mailbox_uid.uid RETURNING uid`
  (a clash on the *other* unique `(user_id, mailbox, uid)` raises 23505 — handled in 3.5).
- **Q62** `DELETE FROM mail_mailbox_uid WHERE user_id = $1 AND mailbox = $2 AND mail_id = $3` — `counters.rs:220` (un-star / un-trash pivot).

### 2.11 `mails` — writes and point reads (`postgres/repositories/mails/core.rs`, `core_query.rs`)
- **Q63** insert (36 bound columns + DB-clock stamp) — `core.rs:340` via `table.rs:277`:
  `INSERT INTO mails (updated, mail_id, user_id, message_id, subject, date, html, text, rfc822_size, from_address, from_text, to_address, to_text, cc_address, cc_text, bcc_address, bcc_text, reply_to_address, reply_to_text, envelope_from, envelope_to, attachments, read, saved, sent, deleted, draft, answered, expunged, insight, uid_domain, modseq, spam_score, spam_reasons, is_spam, in_reply_to, reference_ids) VALUES (CURRENT_TIMESTAMP, $1, …, $36) RETURNING mail_id`
  JSONB params are JSON text or NULL; all NUL characters are stripped first (`core.rs:522`). 23505 => merge path (3.5).
- **Q64** `SELECT * FROM mails WHERE user_id = $1 AND message_id = $2` — `core.rs:584` (full row incl. bodies).
- **Q65** `SELECT * FROM mails WHERE mail_id = $1 AND user_id = $2 LIMIT $3` — `core.rs:592` (HTTP body view; spam training).
- **Q66** projected flag probe `SELECT {read|saved} FROM mails WHERE mail_id = $1 AND user_id = $2 LIMIT $3` — `core.rs:627,650,661,683`.
- **Q67** `UPDATE mails SET envelope_to = $1, updated = CURRENT_TIMESTAMP WHERE user_id = $2 AND message_id = $3` — `core.rs:437` (read-modify-write merge of JSON array done in app).
- **Q68** `UPDATE mails SET {draft = $i}{, is_spam = $i}{, saved = $i}{, deleted = $i}, modseq = $j, updated = CURRENT_TIMESTAMP WHERE user_id = $k AND message_id = $l` — `core.rs:455` (placement flip on merge).
- **Q69** `UPDATE mails SET rfc822_size = $1 WHERE mail_id = $2 AND user_id = $3` (deliberately no `updated` bump) — `core.rs:604`.
- **Q70** compare-and-set flag with modseq stamp — `core.rs:637,669`:
  `UPDATE mails SET read = $1, updated = CURRENT_TIMESTAMP, modseq = $2 WHERE mail_id = $3 AND user_id = $4 AND read = $5 RETURNING mail_id` (same shape with `saved`); zero rows => re-probe Q66.
- **Q71** `DELETE FROM mails WHERE mail_id = $1 AND user_id = $2 RETURNING mail_id` (count) — `core.rs:692` (HTTP delete = hard delete; cascades to mapping rows).
- **Q72** `UPDATE mails SET is_spam = $1, updated = NOW(), modseq = $4 WHERE mail_id = $2 AND user_id = $3 AND expunged = FALSE AND is_spam IS DISTINCT FROM $1 RETURNING mail_id` — `core_query.rs:24` (row_count => "changed").
- **Q73** `SELECT 1 FROM mails WHERE mail_id = $1 AND user_id = $2 AND expunged = FALSE LIMIT 1` — `core_query.rs:33` (row_count => "found").
- **Q74** JSONB containment point lookup — `http/routes/mails/read.rs:89-93` via `table.rs:239`:
  `SELECT * FROM mails WHERE user_id = $1 AND attachments @> $2::jsonb AND expunged = $3 LIMIT $4` with `$2 = [{"content":{"data":"<attachment id>"}}]` (authorizes an attachment download).

### 2.12 `mails` — IMAP reads (`postgres/repositories/mails/imap_query.rs`, `imap.rs`, `counters.rs`)
- **Q75** counts with `FILTER` — `imap_query.rs:48`:
  - domain: `SELECT COUNT(*) FILTER (WHERE {membership}) as total, COUNT(*) FILTER (WHERE read = FALSE AND {membership}) as unread FROM mails WHERE user_id = $1 AND sent = $2 AND expunged = FALSE`
  - mapped: same with `m.` prefix and `FROM mails m JOIN mail_mailbox_uid x ON x.user_id = m.user_id AND x.mailbox = $3 AND x.mail_id = m.mail_id WHERE m.user_id = $1 AND m.sent = $2 AND m.expunged = FALSE`
- **Q76** range read (FETCH, COPY/MOVE source) — `imap_query.rs:103-220`. Projection `{fields}` = any subset of the 40
  `mails` columns (always incl. `mail_id`; `*` = all) plus synthetic `uid_domain AS uid_mailbox` / `x.uid AS uid_mailbox`,
  `octet_length([m.]text) AS text_octets`, `octet_length([m.]html) AS html_octets`.
  - domain, by UID: `SELECT {fields} FROM mails WHERE user_id = $1 AND sent = $2 AND uid_domain >= $3 AND uid_domain <= $4 AND expunged = FALSE{membership}{ AND modseq > $5} ORDER BY uid_domain ASC` ($4 = min(end, 999999999))
  - domain, by sequence: `… WHERE user_id = $1 AND sent = $2 AND expunged = FALSE{membership}{ AND modseq > $5} ORDER BY uid_domain ASC OFFSET $3 LIMIT $4`
  - mapped, by UID: `SELECT {m.fields} FROM mails m JOIN mail_mailbox_uid x ON x.user_id = m.user_id AND x.mailbox = $3 AND x.mail_id = m.mail_id WHERE m.user_id = $1 AND m.sent = $2 AND x.uid >= $4 AND x.uid <= $5 AND m.expunged = FALSE{membership m.}{ AND m.modseq > $6} ORDER BY x.uid ASC`
  - mapped, by sequence: `… WHERE m.user_id = $1 AND m.sent = $2 AND m.expunged = FALSE{membership}{ AND m.modseq > $6} ORDER BY x.uid ASC OFFSET $4 LIMIT $5`
  The IMAP layer (both servers) always resolves sequence numbers to UIDs in memory and calls with `use_uid = true`
  (`imap/message_ops.rs:291,795`; `TS: imap/message-ops.ts:213,846,1055`), so the OFFSET/LIMIT variants are reachable
  only through the builder API. Identical concurrent reads are coalesced in-process (`imap.rs:330`, single-flight).
- **Q77** SEARCH — `imap_query.rs:679-727`:
  `SELECT {U} as uid FROM {mails m | mails m, mail_mailbox_uid x} WHERE m.user_id = $1 AND m.sent = $2 AND m.expunged = FALSE AND {membership_expression m. | TRUE}[ AND x.user_id = m.user_id AND x.mailbox = $3 AND x.mail_id = m.mail_id]{ AND <criterion>}* ORDER BY {U} ASC`
  with `{U}` = `uid_domain` (domain) or `x.uid` (mapped). **No LIMIT.** Criteria are AND-ed; each is one of
  (`imap_query.rs:435-535`, columns unqualified):

  | IMAP key | fragment emitted | params |
  |---|---|---|
  | ALL, OLD, RECENT, UNKEYWORD | (nothing) | |
  | SEEN / UNSEEN, NEW | `read = TRUE` / `read = FALSE` | |
  | FLAGGED / UNFLAGGED | `saved = TRUE` / `saved = FALSE` | |
  | ANSWERED / UNANSWERED | `answered = TRUE` / `answered = FALSE` | |
  | DELETED / UNDELETED | `deleted = TRUE` / `deleted = FALSE` | |
  | DRAFT / UNDRAFT | `draft = TRUE` / `draft = FALSE` | |
  | KEYWORD x, LARGER n, SMALLER n, unknown key, bare sequence set that was not resolved | `FALSE` | |
  | SUBJECT s | `subject ILIKE $n` | `%s%` |
  | FROM / TO / CC / BCC s | `from_text` / `to_text` / `cc_text` / `bcc_text` `ILIKE $n` | `%s%` |
  | BODY s | `text ILIKE $n` | `%s%` |
  | TEXT s | `(subject ILIKE $n OR from_text ILIKE $n OR to_text ILIKE $n OR text ILIKE $n)` (same `$n` x4) | `%s%` |
  | HEADER f s | f in {subject -> `subject`, from -> `from_text`, to -> `to_text`, message-id -> `message_id`}: `{col} ILIKE $n`; any other field: `FALSE` | `%s%` |
  | BEFORE / SENTBEFORE d | `date < $n` | ISO timestamp |
  | ON / SENTON d | `date >= $n AND date < $n+1` | d, d + 24 h |
  | SINCE / SENTSINCE d | `date >= $n` | ISO timestamp |
  | UID set (also sequence sets, resolved by the session) | per range `{U} = $n` or `({U} >= $n AND {U} <= $m)`; several ranges joined `(r1 OR r2 OR …)` | integers |
  | NOT k | `NOT ({k})`; NOT of "no constraint" => `FALSE`; NOT of `FALSE` => no constraint | |
  | OR a b | `({a} OR {b})`; either side unconstrained => no constraint; `FALSE` sides dropped; both `FALSE` => `FALSE` | |

  NOT/OR nest to arbitrary depth (built iteratively; parameters of discarded branches are rolled back so `$n` stays
  dense). An unrepresentable date makes the builder fail and the search return nothing without a statement
  (`imap.rs:635-650`). Things to replicate exactly: the user's string is **not escaped**, so `%`, `_` and `\` in it act
  as LIKE wildcards/escape; `ILIKE` case-folding is locale dependent; SQL three-valued logic applies
  (`NOT (cc_text ILIKE …)` is not true for a NULL `cc_text`, so such a mail is *not* returned).
- **Q78** all UIDs (the session's sequence-number map; SELECT, IDLE wake-ups, after EXPUNGE/MOVE/APPEND) — `imap_query.rs:735`:
  `SELECT uid_domain as uid FROM mails WHERE user_id = $1 AND sent = $2 AND expunged = FALSE{membership} ORDER BY uid_domain ASC` /
  `SELECT x.uid as uid FROM mails m JOIN mail_mailbox_uid x ON … x.mailbox = $3 … WHERE m.user_id = $1 AND m.sent = $2 AND m.expunged = FALSE{membership m.} ORDER BY x.uid ASC` — unbounded result.
- **Q79** first unseen — `imap_query.rs:756`: Q78 plus `AND [m.]read = FALSE` and `LIMIT 1`.
- **Q80** flags of named mails (IDLE flag push) — `imap_query.rs:906`:
  `SELECT mail_id AS mail_id, uid_domain AS uid, read, saved, deleted, draft, answered FROM mails WHERE user_id = $1 AND sent = $2 AND expunged = FALSE AND mail_id = ANY($3)` /
  mapped: `SELECT m.mail_id AS mail_id, x.uid AS uid, m.read, m.saved, m.deleted, m.draft, m.answered FROM mails m JOIN mail_mailbox_uid x ON … WHERE m.user_id = $1 AND m.sent = $2 AND m.expunged = FALSE AND m.mail_id = ANY($4)` (array param, type inferred from the column).
- **Q81** HIGHESTMODSEQ — `counters.rs:323`:
  `SELECT COALESCE(MAX(modseq), 1) AS highest FROM mails WHERE user_id = $1 AND sent = $2` /
  `SELECT COALESCE(MAX(m.modseq), 1) AS highest FROM mails m JOIN mail_mailbox_uid x ON x.user_id = m.user_id AND x.mailbox = $3 AND x.mail_id = m.mail_id WHERE m.user_id = $1 AND m.sent = $2`
  (no `expunged` and no membership filter on purpose: expunges must advance it).
- **Q82** chunked body read by **characters** — `imap.rs:120`:
  `SELECT SUBSTRING({text|html} FROM $3::int FOR $4::int) AS chunk FROM mails WHERE mail_id = $1 AND user_id = $2` (12 000 chars per round trip; loop until a short/empty chunk).
- **Q83** chunked body read by **bytes** with seek — `imap.rs:190`:
  `SELECT SUBSTRING(convert_to({text|html}, 'UTF8') FROM $3::int FOR $4::int) AS chunk FROM mails WHERE mail_id = $1 AND user_id = $2` (48 KiB per round trip, arbitrary 1-based start byte; result is `bytea`).

### 2.13 `mails` — IMAP writes
STORE (`postgres/repositories/mails/set_flags_query.rs:39-132`, driver `imap.rs:547-626`). `{set}` (`imap.rs:484`):
`+FLAGS` -> subset of `read = TRUE, saved = TRUE, deleted = TRUE, draft = TRUE, answered = TRUE` (column order; empty =>
no UPDATE at all); `-FLAGS` -> same with `FALSE`; `FLAGS` -> all five assigned `true|false`. `{W}` =
- domain/UID: `user_id = $1 AND sent = $2 AND uid_domain >= $3 AND uid_domain <= $4 AND expunged = FALSE{membership}`
- domain/seq: `mail_id IN (SELECT mail_id FROM mails WHERE user_id = $1 AND sent = $2 AND expunged = FALSE{membership} ORDER BY uid_domain ASC OFFSET $3 LIMIT $4)`
- mapped/UID: `m.user_id = $1 AND m.sent = $2 AND x.user_id = m.user_id AND x.mailbox = $3 AND x.mail_id = m.mail_id AND x.uid >= $4 AND x.uid <= $5 AND m.expunged = FALSE{membership m.}`
- mapped/seq: `m.user_id = $1 AND m.sent = $2 AND x.user_id = m.user_id AND x.mailbox = $3 AND x.mail_id = m.mail_id AND m.mail_id IN (SELECT y.mail_id FROM mail_mailbox_uid y JOIN mails z ON z.user_id = y.user_id AND z.mail_id = y.mail_id AND z.sent = $2 AND z.expunged = FALSE AND {membership_expression z. | TRUE} WHERE y.user_id = $1 AND y.mailbox = $3 ORDER BY y.uid ASC OFFSET $4 LIMIT $5)`

- **Q84** current-state select (no-op STORE): `SELECT mail_id, uid_domain as uid, read, saved, deleted, draft, answered, modseq as modseq FROM mails WHERE {W}` / `SELECT m.mail_id, x.uid as uid, m.read, m.saved, m.deleted, m.draft, m.answered, m.modseq as modseq FROM mails m, mail_mailbox_uid x WHERE {W}`.
- **Q85** matched UIDs (conditional STORE only): `SELECT uid_domain as uid FROM mails WHERE {W}` / `SELECT x.uid as uid FROM mails m, mail_mailbox_uid x WHERE {W}`.
- **Q86** the update, returning post-image:
  `UPDATE mails SET {set}, updated = CURRENT_TIMESTAMP, modseq = $5 WHERE {W}[ AND modseq <= $6] RETURNING mail_id, uid_domain as uid, read, saved, deleted, draft, answered, modseq as modseq` /
  `UPDATE mails m SET {set}, updated = CURRENT_TIMESTAMP, modseq = $6 FROM mail_mailbox_uid x WHERE {W}[ AND m.modseq <= $7] RETURNING m.mail_id, x.uid as uid, m.read, m.saved, m.deleted, m.draft, m.answered, m.modseq as modseq`
  (`UPDATE … FROM` join; RETURNING a column of the joined table; every matched row gets the new modseq even if its
  flags did not change; the UNCHANGEDSINCE guard is the optional last predicate. Range params are bound the way JS prints
  numbers, so `Infinity`/`1e+21` produce a server error that is absorbed as "addressed nothing".)

EXPUNGE / MOVE-source removal (soft delete; `imap.rs:728-835`, builders `imap_query.rs:779-896`):
- **Q87** domain views, single UPDATE via `Table::update_where` (`table.rs:481`):
  - EXPUNGE on unified Sent: `UPDATE mails SET expunged = $1, updated = CURRENT_TIMESTAMP, modseq = $2 WHERE user_id = $3 AND sent = $4 AND deleted = $5 AND expunged = $6 RETURNING uid_domain as uid` (`imap_query.rs:797`)
  - MOVE source on any domain view: `UPDATE mails SET expunged = $1, updated = CURRENT_TIMESTAMP, modseq = $2 WHERE user_id = $3 AND sent = $4 AND expunged = $5 AND uid_domain IN ($6, …)[ AND is_spam = $ AND draft = $ | AND draft = $ | AND is_spam = $] RETURNING uid_domain as uid` (`imap_query.rs:861`)
- **Q88** EXPUNGE on INBOX / Drafts / Junk – candidate lookup with correlated subqueries (`imap_query.rs:829`):
  ```sql
  SELECT m.mail_id as mail_id, m.uid_domain as uid FROM mails m
  WHERE m.user_id = $1 AND m.sent = $2 AND m.deleted = TRUE AND m.expunged = FALSE{membership m.}
    AND ( EXISTS (SELECT 1 FROM mail_mailbox_uid x WHERE x.user_id = m.user_id AND x.mail_id = m.mail_id
                    AND (x.mailbox = $3 OR x.mailbox LIKE $4))
          OR ( m.message_id NOT LIKE $5
               AND NOT EXISTS (SELECT 1 FROM mail_mailbox_uid x WHERE x.user_id = m.user_id AND x.mail_id = m.mail_id
                                 AND x.mailbox NOT IN ($6, $7)) ) )
  -- $3='INBOX', $4='INBOX/accounts/%', $5='%.copy@server', $6='Starred', $7='Trash'
  ```
- **Q89** EXPUNGE on a mapped box – lookup (`imap_query.rs:849`): `SELECT m.mail_id as mail_id, x.uid as uid FROM mails m JOIN mail_mailbox_uid x ON x.user_id = m.user_id AND x.mailbox = $3 AND x.mail_id = m.mail_id WHERE m.user_id = $1 AND m.sent = $2 AND m.deleted = TRUE AND m.expunged = FALSE{membership m.}`
- **Q90** MOVE source on a mapped box – lookup (`imap_query.rs:880`): same join `WHERE m.user_id = $1 AND m.sent = $2 AND x.uid IN ($4,…) AND m.expunged = FALSE{membership m.}`.
- **Q91** mark step after Q88–Q90 (`imap.rs:750-779`):
  `UPDATE mails SET expunged = $1, updated = CURRENT_TIMESTAMP, modseq = $2 WHERE user_id = $3 AND mail_id IN ($4, …) AND deleted = $n AND expunged = $n+1 RETURNING mail_id` (EXPUNGE: re-checks the row is still `\Deleted`) /
  `… WHERE mail_id IN ($3, …) RETURNING mail_id` (MOVE: unconditional). Returned ids are mapped back to UIDs from the lookup.
  Mapping rows are **kept** on expunge (UID stays retired).

COPY / MOVE / APPEND have no dedicated SQL: per message they run Q76 (source row with full bodies) -> Q59 x2 (domain +
account/mailbox UID) -> Q59 (modseq) -> Q63 -> Q61 x1–2 (in parallel) -> optional Q59+Q61 / Q62 pivots, and MOVE ends with
Q87/Q90+Q91 and Q78 (`imap/message_ops.rs:660-936`, `core.rs:313-373`). The copy gets a deterministic Message-ID
`<sha256(source id \0 destination)[..16]>.copy@server` so a retried COPY hits the `(user_id, message_id)` unique and
merges.

### 2.14 `mails` — HTTP reads (`postgres/repositories/mails/http_query.rs`, `http.rs`, `mails/notifications.rs`)
`{H}` (`http_query.rs:69`) = `mail_id, user_id, subject, date, from_address, from_text, to_address, to_text, cc_address, cc_text, bcc_address, bcc_text, envelope_to, read, saved, sent, is_spam, insight, left(text, 400) AS text_prefix, message_id, in_reply_to, CASE WHEN jsonb_array_length(reference_ids) > 3 THEN jsonb_build_array(reference_ids->0, reference_ids->-2, reference_ids->-1) ELSE reference_ids END AS reference_ids`.
`{A}` (`http_query.rs:77`) = `from_address @> $2::jsonb` (sent) | `(to_address @> $2::jsonb OR cc_address @> $2::jsonb OR bcc_address @> $2::jsonb OR envelope_to @> $2::jsonb)` (received) | `({sent} OR {received})` (saved && !sent); `$2 = [{"address":"<addr>"}]`.
`{UA}` (`http_query.rs:200`) = `EXISTS (SELECT 1 FROM jsonb_array_elements(COALESCE(to_address, '[]'::jsonb) || COALESCE(cc_address, '[]'::jsonb) || COALESCE(bcc_address, '[]'::jsonb) || COALESCE(envelope_to, '[]'::jsonb)) AS recipient WHERE recipient->>'address' = ANY($2::text[]))`.

- **Q92** header rows by id batch (500 ids per statement, re-ordered in app) — `http_query.rs:90`:
  `SELECT {H} FROM mails WHERE user_id = $1 AND mail_id = ANY($2::uuid[]) AND expunged = FALSE`
- **Q93** per-account list — `http_query.rs:104`:
  `SELECT {H | mail_id | mail_id, date::text AS cursor_date} FROM mails WHERE user_id = $1 AND {A} AND expunged = FALSE AND draft = FALSE[ AND saved = TRUE]{ AND is_spam = TRUE AND sent = FALSE | AND is_spam = FALSE}[ AND updated > $n][ AND (date, mail_id) < ($n::timestamptz, $n+1::uuid)] ORDER BY date DESC, mail_id DESC[ LIMIT $n][ OFFSET $n]`
  Used as: full id list (`LIMIT 10000 OFFSET 0`, `mails/headers.rs:195`), delta rows (`updated > since`, no limit, full `{H}`), id page (**Q94**).
- **Q94** keyset page — `http_query.rs:158`: Q93 projecting `mail_id, date::text AS cursor_date`, row-value comparison
  `(date, mail_id) < ($3::timestamptz, $4::uuid)`, `LIMIT limit+1` (the extra row tells whether a next page exists; cursor =
  microsecond `date::text` + id of the last row, base64url-encoded by the route). limit <= 10 000.
- **Q95** delta watermark from the DB clock — `http_query.rs:168`: `SELECT now() - make_interval(secs => $1) AS as_of` ($1 = 2).
- **Q96** delta evictions — `http_query.rs:175`: `SELECT mail_id FROM mails WHERE user_id = $1 AND {A} AND {(expunged = TRUE OR is_spam = TRUE) | (expunged = TRUE OR is_spam = FALSE)} AND updated > $3`.
- **Q97** unified list — `http_query.rs:206`: `SELECT {H | mail_id | mail_id, date::text AS cursor_date} FROM mails WHERE user_id = $1 AND {UA} AND expunged = FALSE AND draft = FALSE AND is_spam = FALSE[ AND updated > $n]{[ AND (date, mail_id) < ($n::timestamptz, $n+1::uuid)] ORDER BY date DESC, mail_id DESC | ORDER BY read ASC, date DESC}[ LIMIT $n][ OFFSET $n]` (unpaged = unread first then newest, `LIMIT 10000 OFFSET 0`).
- **Q98** unified keyset page — `http_query.rs:256` (as Q94 over Q97).
- **Q99** unified evictions — `http_query.rs:266`: `SELECT mail_id FROM mails WHERE user_id = $1 AND {UA} AND (expunged = TRUE OR is_spam = TRUE) AND updated > $3`.
- **Q100** full-text search — `http_query.rs:282`:
  ```sql
  SELECT {H},
    ts_rank(search_vector, plainto_tsquery('english', $2)) as rank,
    ts_headline('english', subject, plainto_tsquery('english', $2), 'StartSel=<em>, StopSel=</em>, MaxWords=50, MinWords=10') as subject_highlight,
    ts_headline('english', text,    plainto_tsquery('english', $2), 'StartSel=<em>, StopSel=</em>, MaxWords=50, MinWords=10') as text_highlight
  FROM mails
  WHERE user_id = $1 AND search_vector @@ plainto_tsquery('english', $2) AND expunged = FALSE AND draft = FALSE
  ORDER BY rank DESC, date DESC LIMIT 1000
  ```
  (spam included and flagged in the response; a highlight is used only if it contains `<em>`).
- **Q101** account sidebar aggregation (CTE + set-returning unnest + DISTINCT + GROUP BY) — `http_query.rs:306`:
  ```sql
  WITH expanded_mails AS (
    SELECT DISTINCT mail_id, read, saved, date, {E}
    FROM mails
    WHERE user_id = $1 AND expunged = FALSE AND draft = FALSE {S} AND {N})
  SELECT address, COUNT(*) as count,
         SUM(CASE WHEN read = FALSE THEN 1 ELSE 0 END) as unread,
         SUM(CASE WHEN saved = TRUE THEN 1 ELSE 0 END) as saved_count,
         MAX(date) as latest
  FROM expanded_mails
  WHERE address IS NOT NULL [AND address ILIKE '%@' || $2]
  GROUP BY address ORDER BY latest DESC
  ```
  `{E}`/`{N}` received or spam: `jsonb_array_elements(COALESCE(to_address,'[]'::jsonb) || COALESCE(cc_address,'[]'::jsonb) || COALESCE(bcc_address,'[]'::jsonb) || COALESCE(envelope_to,'[]'::jsonb))->>'address' as address` / `(to_address IS NOT NULL OR cc_address IS NOT NULL OR bcc_address IS NOT NULL OR envelope_to IS NOT NULL)`;
  sent: `jsonb_array_elements(from_address)->>'address' as address` / `from_address IS NOT NULL`.
  `{S}` = `AND is_spam = FALSE` | `AND is_spam = TRUE AND sent = FALSE`.
  Called 3x per `/accounts` (`mails/accounts.rs:47`) **and 2x for every IMAP LIST/LSUB and every non-INBOX
  SELECT/EXAMINE/STATUS/APPEND/COPY/MOVE existence check** (`imap/store.rs:403,471-526,565`).
- **Q102** same CTE restricted by FTS — `http_query.rs:355`: adds `AND search_vector @@ plainto_tsquery('english', $2)` inside the CTE (received expansion only; domain filter is `$3`).
- **Q103** unread badge — `mails/notifications.rs:26`:
  `SELECT user_id, COUNT(*) FILTER (WHERE read = FALSE) as unread_count, MAX(date) as latest FROM mails WHERE user_id IN ($1, …) AND sent = FALSE AND is_spam = FALSE AND expunged = FALSE AND draft = FALSE GROUP BY user_id`

### 2.15 Conversation threading (`postgres/repositories/mails/thread.rs`)
`{T}` = `mail_id, message_id, in_reply_to, reference_ids, subject, date, from_address, from_text, to_address, to_text, cc_address, cc_text, bcc_address, bcc_text, envelope_to, read, saved, sent, is_spam, insight, left(text, 400) AS text_prefix`.
`{NS}` = `lower(btrim(regexp_replace(regexp_replace(subject, '^\s*((re|fwd?|aw|wg|sv|vs|antw|tr|rv)\s*(\[\d+\])?\s*[:：]\s*)+', '', 'i'), '\s+', ' ', 'g')))`.
- **Q104** seed — `:113`: `SELECT {T} FROM mails WHERE user_id = $1 AND mail_id = $2 AND expunged = FALSE`.
- **Q105** link candidates (run up to 4 rounds, widening the id set) — `:125`:
  `SELECT {T} FROM mails WHERE user_id = $1 AND expunged = FALSE AND draft = FALSE AND message_id NOT LIKE '%.copy@server' [AND is_spam = FALSE] AND (message_id = ANY($2::text[]) OR in_reply_to = ANY($2::text[]) OR (reference_ids IS NOT NULL AND reference_ids ?| $2::text[])) ORDER BY date DESC LIMIT 300` ($2 = each id bare and `<bracketed>`).
- **Q106** header-less subject match (CTE scalar reused; must hit the partial expression index) — `:149`:
  `WITH seed AS (SELECT {NS} AS subject FROM mails WHERE user_id = $1 AND mail_id = $2) SELECT {T} FROM mails WHERE user_id = $1 AND in_reply_to IS NULL AND reference_ids IS NULL AND md5({NS}) = md5((SELECT subject FROM seed)) AND {NS} = (SELECT subject FROM seed) AND expunged = FALSE AND draft = FALSE AND message_id NOT LIKE '%.copy@server' [AND is_spam = FALSE] ORDER BY date DESC LIMIT 100`
- **Q107** `SELECT reference_ids FROM mails WHERE user_id = $1 AND message_id = ANY($2::text[]) AND expunged = FALSE LIMIT 1` — `:176` (building References for a reply).

### 2.16 Generic `Table` wrapper capabilities (`postgres/repositories/mails/table.rs`; `TS: postgres/models/base.ts`, `database.ts`)
What the wrapper can emit (superset of what callers use): equality filters (`=`; NULL value => `IS NULL` in SELECT,
`= NULL` in mutations); `col @> $n::jsonb`; `IS NULL`; `IS NOT TRUE`; `<, <=, >, >=` with optional `IS NOT NULL` guard;
`IN ($…)` (empty list refused); `LIMIT $n`; `COUNT(*)::int`; INSERT/UPDATE/DELETE with `RETURNING`; `updated =
CURRENT_TIMESTAMP` stamping only for tables that declare `updated`; a guard that refuses any UPDATE/DELETE with a
missing or empty filter set (`table.rs:148`). TS additionally has (unused by production paths): `ORDER BY`, `OFFSET`,
date-range and IN filters in `buildSelectWithFilters`, `queryByIds`, `softDelete`.

---

## 3. Transactions and concurrency

### 3.1 Explicit transactions
Exactly one: boot-time migration (`postgres/migration.rs:177-219`, `TS: postgres/migration.ts:356-415`) on a dedicated
connection: `pg_advisory_lock(6908002)` -> `BEGIN` -> for each table Q4 + zero or more Q5 (transactional DDL) -> `COMMIT`
(or `ROLLBACK`) -> `pg_advisory_unlock`. No isolation level is ever set (READ COMMITTED default). A connection dropped
with an open transaction is closed rather than returned to the pool (`pool.rs:558-566`).

### 3.2 Everything else is autocommit, one statement per round trip
Multi-statement operations are deliberately **not** atomic; statements of one logical operation run on *different*
pooled connections (often concurrently via `join!`/`join_all`), so the database must give read-your-writes across
connections immediately after each statement returns. Sequences that are grouped only by app logic:
- **Save a mail** (`core.rs:313-373`): Q59 modseq -> Q63 -> {Q61 mapped box, Q61 `INBOX` marker} in parallel -> pivots
  (Q59+Q61 per `Starred`/`Trash`). Callers reserve the two UIDs (Q59 x2, parallel) before that.
- **Duplicate delivery merge** (`core.rs:428-493`): Q64 -> Q67 -> [Q59 + Q68] -> Q61s -> pivots.
- **STORE** (`imap.rs:577-619`): Q59 -> [Q85] -> Q86 -> per updated row and touched flag Q59+Q61 or Q62 (`imap/message_ops.rs:547-558`).
- **EXPUNGE / MOVE** (`imap.rs:750-835`): Q88/Q89/Q90 -> Q59 -> Q91; then Q78 to rebuild sequence numbers.
- **COPY/MOVE**: per source message as in 2.13; a failure leaves earlier copies in place ("partially failed").
- **mark read / saved** (`core.rs:620-688`): Q66 -> Q59 -> Q70 -> [Q66] -> pivot.
- **mark spam** (`core.rs:707`): Q59 -> Q72 -> [Q73].
- **CREATE mailbox** (`mailboxes.rs:221`): Q33 -> Q35 -> Q36 (check-then-insert; no unique constraint backs it).
- **RENAME** (`mailboxes.rs:280`): Q33 -> Q33 -> Q38. **DELETE**: Q33 -> Q37. **SUBSCRIBE**: Q33 -> Q39.
- **Session read** (`postgres/repositories/sessions.rs:192`): Q27 -> Q17 -> [Q29]. **Password reset** (crate-root `users.rs:185-255`): Q17 x1–2 -> Q31 -> Q19.
- **UIDVALIDITY** (`postgres/repositories/users.rs:259`): Q17 -> Q20 (read-then-write lazy init; last writer wins).

### 3.3 Row locks, advisory locks, isolation
- No `SELECT … FOR UPDATE`, `SKIP LOCKED`, `NOWAIT`, `LOCK TABLE`, `SET TRANSACTION`, or `lock_timeout` anywhere.
- Session advisory locks: `6908002` (blocking; serialises migrations across instances) and `6908003`
  (`pg_try_advisory_lock`; a second instance skips boot maintenance) — `migration.rs:15`, `maintenance.rs:26`.
- Implicit reliance on single-statement atomicity and row-level write locks of Postgres for: `ON CONFLICT DO UPDATE`
  counters (Q59), additive upserts (Q56), compare-and-set updates (Q24, Q70, Q72, Q86 guard, Q91 guard), capped insert
  (Q50 – count and insert are one statement, but nothing serialises two concurrent adds of *different* patterns, so the
  cap is approximate under races), and `password_version = COALESCE(password_version,1)+1` (Q19, Q26).

### 3.4 How UID / UIDNEXT / UIDVALIDITY / MODSEQ are made safe
- **Allocation** = Q59, a single `INSERT … ON CONFLICT (pk) DO UPDATE SET last_uid = last_uid + 1 RETURNING`. First use
  seeds from `MAX()+1` of existing data; concurrent first uses are arbitrated by the PK (loser takes the DO UPDATE
  branch). Values are strictly increasing per key and never reused; gaps are fine and common (a reservation is burned
  when the later insert merges, when a pivot already exists, or when a guarded update matches nothing).
- **Keys**: `(user, 'domain', '', sent)` -> `mails.uid_domain` for INBOX/Drafts/Junk (sent=false) and unified Sent
  (sent=true); `(user, 'account', <address>, sent)` -> `mail_mailbox_uid.uid` for `INBOX/accounts/<local>`,
  `Sent Messages/accounts/<local>` **and user-created boxes** (address = `<box>@<user domain>`); `(user, 'mailbox',
  'Starred'|'Trash', false)`; `(user, 'modseq', '', false)`.
- **UIDNEXT** = Q60 (counter + 1, or the same seed if the counter row does not exist yet) — never `MAX(uid)` of surviving rows.
- **Per-mailbox UID persistence** = Q61: PK `(user_id, mailbox, mail_id)` makes it idempotent and returns the UID that
  actually stuck; UNIQUE `(user_id, mailbox, uid)` rejects a duplicate UID.
- **UIDVALIDITY** = `users.imap_uid_validity` (one value per user for all mailboxes), unix seconds, set once.
- **MODSEQ**: one per-user counter; every insert, flag change, spam/placement flip and expunge stamps a freshly reserved
  value (one value per STORE/EXPUNGE statement, shared by all rows it touches). HIGHESTMODSEQ = Q81 (`MAX` over rows,
  floor 1). CHANGEDSINCE = `modseq > $n` (Q76). UNCHANGEDSINCE = `modseq <= $n` guard inside Q86 plus Q85 to compute the
  MODIFIED set as matched − updated.
- **Sequence numbers** are not computed in SQL (no `row_number()`): the session loads all UIDs (Q78) and keeps
  `seq_to_uid` / `uid_to_seq` in memory, rebuilding after EXPUNGE/MOVE/APPEND/IDLE notifications.

### 3.5 Error codes the app branches on
`DbError` carries SQLSTATE `code`, `detail`, `constraint`, `table`, `column` (`pool.rs:145-200`). Only **23505
(unique_violation)** changes behaviour:
1. `save_mail` (`core.rs:568-579`, `TS: core.ts:363`): a 23505 from anywhere in the insert path (the `mails`
   `(user_id, message_id)` unique, or the mapped box's `(user_id, mailbox, uid)` unique) switches to
   `merge_into_existing`; a 23505 during the merge is a hard error.
2. `write_one_mapping` (`core.rs:164-189`): 23505 on the `INBOX` scope-marker row (domain scope; legacy duplicate
   `uid_domain` values) is logged and skipped; on the mapped destination it aborts the save.
3. `link_user_auth_sub` (`users.rs:296`): 23505 on `auth_sub` => "someone else linked first" (`None`).
4. Everything else uses `ON CONFLICT`: `(user_id)`, `(session_id)`, `(key)`, `(user_id, uid_kind, uid_scope, sent)`,
   `(user_id, word)` DO UPDATE; `(user_id, mailbox, mail_id)` DO UPDATE no-op (to get RETURNING); `(event_id)`,
   `(user_id, pattern)` DO NOTHING; target-less DO NOTHING (Q25).
No handling of 40001/40P01 (serialization/deadlock) and **no statement retries** anywhere. Other codes only appear as
generic faults: 57014 (statement timeout/cancel), 22P02 (malformed number bound by STORE), 22001 (over-long
`spam_training.word`). When a save fails, `code` and `constraint` are written to `./error/<ms>` (`mails/receive.rs:403`).
Many read paths *absorb* any DB error into an empty answer (IMAP counts, UID lists, range reads, SEARCH, STORE, EXPUNGE,
push repository, FTS, account stats); `get_uid_next`, `expunge_mails_by_uid`, `mailbox_exists`, chunk readers, session
store, and all saves propagate.

### 3.6 Row count / RETURNING the app relies on
Q3–Q5 none; Q19 (new `password_version`), Q20 (exists?), Q22, Q24/Q25 (row or nothing), Q26, Q29–Q31 (deleted count),
Q36 (`RETURNING *`), Q37, Q39, Q40–Q46, Q50 (created row) + Q51 (row_count), Q52/Q53 (affected > 0), Q59/Q61 (value),
Q63 (`mail_id`), Q70 (matched?), Q71 (count), Q72/Q73 (row_count), Q86 (post-image rows incl. joined `x.uid`), Q87/Q91
(ids/uids actually marked), Q14 (rows affected drives the drain loop). The pool reports SELECT row counts too.

### 3.7 Timeouts, pool, cancellation
- Pool: `max = 10`, connect timeout 2 s, no checkout wait limit (`postgres/mod.rs:21-35`; TS identical plus 30 s idle timeout).
- `statement_timeout = 30000` ms set as a startup option on every connection; client-side 30 s timer that sends a
  **cancel request** and fails the call with "Query read timeout" (`pool.rs:387-427`).
- Maintenance session: `SET statement_timeout = 600000`, client limit 630 s, per-statement 10 min drain deadline, cancelled
  on shutdown by `pg_cancel_backend(pid)` from another connection, connection destroyed afterwards (`maintenance.rs`).
- Boot waits for the database with 30 attempts x 2 s (`initialize.rs:28`).

### 3.8 In-process coordination that stands in for DB features
- Single-flight coalescing of identical range reads (`mails/inflight.rs`).
- IMAP IDLE, session eviction, auth rate limits, HTTP rate limits: all in process memory.

---

## 4. Other server features relied upon

- **LISTEN / NOTIFY: not used.** IMAP IDLE is an in-process registry (`imap/idle_manager.rs`); SMTP delivery / HTTP flag
  changes call `idle_manager::notify_new_mail` / `notify_flag_change` directly (`push.rs:430,472`), and each woken session
  re-reads Q78 / Q80. A second process would not wake another process's IDLE clients. The only cross-process mechanisms
  are the two advisory locks, the `schema_meta` markers, and the `password_version` fence read on every IMAP/SMTP command
  and HTTP session load.
- **Full-text search (HTTP only)**: `tsvector` column built by trigger with `to_tsvector('english', subject' ' text ' '
  from_text ' ' to_text)` (angle brackets in subject/from/to replaced by spaces); query `plainto_tsquery('english', $2)`
  (AND of stemmed terms, stop-words dropped); match `@@`; ranking `ts_rank` (default weights/normalisation) then `date
  DESC`; highlighting `ts_headline('english', …, 'StartSel=<em>, StopSel=</em>, MaxWords=50, MinWords=10')` on `subject`
  and on the full `text`; `LIMIT 1000`; GIN index. No `websearch_to_tsquery`/`to_tsquery`, no weights (`setweight`), no
  phrase search. Postgres caps a tsvector at ~1 MB; an INSERT whose text exceeds that would fail in the trigger.
- **IMAP SEARCH**: unindexed `ILIKE '%…%'` scans over `subject`, `*_text`, `message_id`, and the full `text` body; date and
  flag predicates; UID ranges. No tsvector use.
- **Sequences**: none; counters are table rows (Q59). **Server-generated ids**: `gen_random_uuid()` defaults on
  `users` (some paths), `mailboxes`, `spam_allowlist`, `spam_training`; app-generated UUIDv4 for `mails`,
  `push_subscriptions`, pending users; random strings for `sessions`.
- **Server time**: `CURRENT_TIMESTAMP` / `NOW()` for every `updated` stamp and column defaults; `now() -
  make_interval(secs => 2)` as the delta-sync watermark (Q95). The design explicitly requires `updated` and the watermark
  to come from the same (DB) clock. App clock is used for `date` fallbacks, `last_notified`, session expiry comparisons and
  the 7-day push cutoff.
- **Blobs**: no `bytea` columns. Message bodies are `TEXT` (`text`, `html`), read whole (Q65, COPY) or in ranges
  (Q82/Q83) and measured with `octet_length()` without loading (Q76). Attachment bytes are files under `./attachments/<id>`
  (hard-linked/copied on save); only JSON metadata is in the row. Raw RFC 822 messages are not stored; IMAP `BODY[]` is
  re-synthesised from columns + files. NUL characters are stripped from all text/JSON before insert because Postgres
  cannot store them.
- **JSONB**: operators/functions used — `@>` (containment of `[{"address":…}]` in address arrays; of
  `[{"content":{"data":…}}]` in attachments), `?|` with `text[]` (any of these strings is an element of `reference_ids`),
  `->` with positive and **negative** integer index, `->>'address'`, `||` (array concatenation), `jsonb_array_elements`
  (in the SELECT list and in FROM), `jsonb_array_length`, `jsonb_build_array`, `'[]'::jsonb`, `COALESCE` on jsonb. JSONB
  values are returned verbatim in API responses, so Postgres' canonical form is observable (object keys ordered by
  length then bytes, duplicate keys collapsed, insignificant whitespace dropped); responses carry an ETag hashed from the
  body.
- **Arrays**: only as parameters (`= ANY($n)`, `= ANY($n::uuid[])`, `= ANY($n::text[])`, `<> ALL($n::text[])`, `?|
  $n::text[]`); no array columns.
- **Collation / case folding**: `LOWER(name) = LOWER($2)` (mailboxes), `LOWER(email) = LOWER($1)` (users),
  `LOWER(pattern) = $2` against an app-lowercased value (the Rust toolchain is pinned so its lowercasing matches Bun's),
  `ILIKE` (SEARCH; `address ILIKE '%@' || $2` for the domain filter, also unescaped), `lower()`+`btrim()`+two
  `regexp_replace` calls with flags `'i'`/`'g'` and POSIX classes `\s`, `\d`, a literal full-width colon, then `md5()`
  (must equal the app's `normalize_subject`). Text equality/grouping (`GROUP BY address`, `message_id = ANY`) is bytewise.
  No SQL `ORDER BY` on text columns; the mailbox list is sorted in the app (ICU). Database locale is whatever
  `postgres:16-alpine` initdb gives.
- **Ordering semantics relied on**: `uuid` total order (bytewise) for `ORDER BY date DESC, mail_id DESC` and the row-value
  comparison `(date, mail_id) < (…)`; `timestamptz` at microsecond precision round-tripped through `date::text` (text
  form depends on the session TimeZone, e.g. `2026-11-15 02:55:04.123456+00`, and is parsed back with `::timestamptz`);
  boolean order `read ASC` (false first); `ORDER BY rank DESC, date DESC`; `ORDER BY latest DESC` on an aggregate.
  Several orderings have no tie-breaker (`ORDER BY date DESC LIMIT 300`, `read ASC, date DESC`).
- **NULL semantics relied on**: `IS DISTINCT FROM`, `IS NOT TRUE`, `(is_deleted IS NULL OR is_deleted = FALSE)`,
  `COALESCE`, `= NULL` matching nothing in guarded mutations, three-valued logic under `NOT (…)`, unique indexes allowing
  many NULLs (`auth_sub`).
- **Aggregates**: `COUNT(*)`, `COUNT(*) FILTER (WHERE …)`, `SUM(CASE …)`, `MAX(date|modseq|uid|uid_domain)`, `GROUP BY
  user_id | address`, `SELECT DISTINCT` over an unnested set, scalar subqueries in VALUES / COALESCE / WHERE, CTEs
  (`WITH`), correlated `EXISTS` / `NOT EXISTS`, `IN (subquery ORDER BY … OFFSET … LIMIT …)`, implicit comma join and
  `JOIN … ON`, `UPDATE … FROM`. No UNION, no window functions, no DISTINCT ON, no recursive CTE, no LATERAL in app code.
- **Cascade deletes relied on**: deleting an unclaimed sign-up user (Q22) and HTTP mail delete (Q71 -> mapping rows).
  IMAP expunge never deletes rows; expunged `mails` rows and their mapping rows are kept forever (no purge job).
- **Result streaming**: none at protocol level — no cursors/portals/COPY; every result set is buffered. Large reads are
  chunked by the app: 500-id batches (Q92), 12 000-char / 48 KiB body slices (Q82/Q83), FETCH caps (50 messages with
  bodies, 500 header-only, unlimited metadata-only — `imap/message_ops.rs:217-223`), unpaged lists capped at 10 000 ids,
  FTS at 1 000 rows, thread candidates at 300/100. Unbounded: Q78 (all UIDs), Q77 (SEARCH), metadata FETCH `1:*`, delta
  rows since a timestamp.

---

## 5. Migrations and lifecycle

### 5.1 Boot sequence (`main.rs:14-33`; TS `start.ts` equivalent)
1. Wait for PostgreSQL (30 x 2 s).
2. `initialize_postgres` (`postgres/initialize.rs:68`): **fast path** — if `schema_meta.schema_hash` equals the hash
   compiled into the binary (sha256 over table definitions + trigger DDL, first 16 hex), skip all DDL. Otherwise: Q15
   `CREATE TABLE IF NOT EXISTS` for the 10 tables in FK order -> `run_migrations` (3.1; additive only: adds missing
   columns, **fails boot on a type mismatch**, warns on extra columns) -> trigger DDL -> write marker Q2/Q3. Failure is
   fatal.
3. Seed/repair `admin` (Q17, Q18, maybe Q31) and `admin-ro` (Q17, Q18, Q31) accounts.
4. Start HTTP, SMTP, IMAP listeners; start hourly session purge (Q30, `sessions.rs:176`) and daily push-subscription
   sweep (Q43, `push.rs:133`); per-user one-hour token timers call Q17/Q20/Q22 (`users.rs:167` crate root).
5. **Boot maintenance**, non-fatal and not awaited (`initialize.rs:86`, `maintenance.rs:170`): skipped if
   `maintenance_hash` matches; else on a dedicated connection: try-lock -> raise `statement_timeout` -> drop invalid
   leftover indexes (Q11/Q12) -> 34 `CREATE INDEX CONCURRENTLY IF NOT EXISTS` -> drain Q14 -> write marker only if
   everything succeeded; otherwise alarm and retry next boot.
6. Shutdown: cancel maintenance, stop servers, `pool().end()`.

There is no migration history table, no down-migrations, and no column drops/renames; `migrations/*.sql` are manual
one-offs. `schema.json` is the single DDL source for the Rust server and must match the TS models (hash-checked), so
either server can boot on a database created by the other.

### 5.2 Connection configuration
`POSTGRES_HOST/PORT/USER/PASSWORD/DATABASE` (defaults localhost/5432/postgres/–/mail); one process-wide pool of 10
(no read/write split, no separate pools; maintenance and migration borrow one connection each), no TLS to the DB
(`NoTls`), unprepared statements, text parameters.

### 5.3 Multi-process assumptions
- Designed to tolerate several instances **for schema work** (advisory locks, markers) and **credential revocation**
  (`password_version` checked against the DB on every IMAP command `imap/session.rs:881`, SMTP command
  `smtp/handler.rs:304`, and HTTP session load `sessions.rs:208-232`).
- The Bun and Rust servers may run against the same database (rolling swap), so on-disk formats (JSON shapes, cookie
  JSON-in-varchar, bcrypt hashes, counter rows) are shared contracts.
- Not multi-process safe: IDLE wake-ups, rate limits, single-flight, token expiry timers (all in memory). Compose runs a
  single `mail` container.

### 5.4 Periodic jobs
Hourly: delete expired sessions. Daily: delete push subscriptions not refreshed for 7 days. Per sign-up: delete the
placeholder user after 1 h. No VACUUM/ANALYZE/REINDEX issued by the app (autovacuum assumed); the only bulk rewrite is
the boot-time search-vector drain.

---

## 6. Scale hints

- **`mails`** is the only large table. One row per message per user, plus one full clone (bodies included) per IMAP
  COPY/MOVE, plus soft-deleted rows that are never purged. Row payload: `text` + `html` decoded bodies (messages are
  accepted up to 35 MiB over SMTP/IMAP APPEND, `imap/handler.rs:45`; typical tens of KB), a tsvector proportional to the
  body's vocabulary (<= 1 MB), 7 small JSONB documents, ~25 scalar columns. Attachments (<= 25 MiB each, <= 20 per sent
  mail) are on disk. Reads almost never want the bodies: lists use `left(text, 400)`, IMAP uses `octet_length` + ranged
  reads; a store with separate/lazy large-value storage and cheap prefix/range/length access is important.
- **`mail_mailbox_uid`**: 1–4 rows per mail (account box, `INBOX` marker, `Starred`, `Trash`), never removed on expunge.
- **`mail_uid_counters`**: a handful of rows per user; the `modseq` row is written on *every* mutation of that user's
  mail and the `domain`/`account` rows on every delivery — the per-user write hot spot.
- **`spam_training`**: one row per (user, word) — up to the user's vocabulary; each trained document upserts <= 501 rows
  in one statement; each received mail reads <= 501 words by `IN`.
- **`sessions`**: one per signed-in browser, **upserted on every HTTP request** (rolling expiry) after being read
  together with the user row. `spam_allowlist` <= 1000/user, `mailboxes` <= 1000/user, `push_subscriptions` small,
  `mailgun_events` append-only (<= 16 KiB raw each, never pruned), `users` small.
- **Hot paths**
  - Every HTTP API request: Q27 + Q17(user_id) [+ Q28].
  - Every authenticated IMAP command and SMTP command: Q17(username).
  - Inbound SMTP delivery, per local recipient: Q17, Q59 x2, Q49, Q55, Q57, Q59(modseq), Q63 (+trigger tsvector), Q61 x2,
    then per IDLE session of that user Q78 (full UID list) and for web push Q17, Q103, Q44, Q46.
  - IMAP SELECT: [Q101 x2 + Q32 unless INBOX] -> Q78 -> Q75 -> {Q17 (+Q20), Q79, Q60, Q81} in parallel. STATUS (clients
    poll it for every folder): [Q101 x2 + Q32] -> Q75 -> optional Q17/Q81/Q60. LIST/LSUB: Q101 x2 + Q32.
  - IMAP FETCH: one Q76 per UID range (ranges of one command run concurrently); body sections then stream with repeated
    Q82/Q83; a body fetch also triggers mark-read (Q66, Q59, Q70) and may write Q69. Metadata-only `1:*` fetches return a
    row per message in the mailbox.
  - IMAP STORE over ranges: Q59 + Q86 (+ Q85), then 2 statements per row per pivot flag.
  - IMAP SEARCH: Q77 with substring scans over bodies.
  - HTTP mailbox list: Q93 ids (GIN containment on address JSONB + sort by date) then Q92 batches; sidebar Q101 x3 per
    load (unnests every recipient of every mail of the user); unified inbox Q97 (EXISTS over unnest — no index
    possible); delta polling Q95 + Q93(`updated >`) + Q96 (no index on `updated`).
  - Thread view: Q104 + up to 4 x Q105 + Q106.
- **Indexes vs access patterns**: almost every query is `user_id = $1` plus booleans; the per-boolean single-column
  indexes are near-useless, and UID-ordered range scans have no composite support in `mails` (only
  `mail_mailbox_uid`'s `(user_id, mailbox, uid)` unique serves mapped boxes). A replacement should offer per-user
  ordered secondary keys: `(user_id, sent, uid_domain)`, `(user_id, mailbox, uid)`, `(user_id, message_id)` unique,
  `(user_id, date DESC, mail_id DESC)`, `(user_id, updated)`, `(user_id, in_reply_to)`, inverted indexes on address
  lists, reference ids, and the full-text vector, plus the subject-key index for header-less threading.

---

## 7. Hardest things to replicate without SQL (ranked)

1. **IMAP mailbox model as ad-hoc relational views**: two UID spaces (predicate over `mails.uid_domain` vs join to
   `mail_mailbox_uid`), each needing ordered range scans, `COUNT … FILTER`, min-unseen, max-modseq, and the dynamically
   composed SEARCH predicate tree (unbounded NOT/OR nesting, AND of criteria, unescaped case-insensitive substring match
   on bodies, date half-open ranges, UID range unions, SQL NULL three-valued logic) — Q75–Q81, Q84–Q91.
2. **Lock-free atomic allocation and compare-and-set in single statements**: upsert-with-seed-subquery-and-RETURNING
   counters (Q59/Q60), idempotent mapping upsert that returns the surviving value (Q61), conditional updates whose
   affected rows/post-images drive protocol answers (Q70, Q72, Q86 with CONDSTORE guard, Q91), `UPDATE … FROM` join
   returning joined columns, additive multi-row upsert (Q56), capped `INSERT … SELECT … WHERE (count) < n ON CONFLICT DO
   NOTHING` (Q50) — all without transactions, across different connections, with 23505 as a control-flow signal (merge on
   duplicate Message-ID; skip duplicate INBOX marker).
3. **Full-text search**: English stemming/stop-word tsvector maintained on write, `plainto_tsquery` semantics, `ts_rank`
   ordering, and `ts_headline` snippets with `<em>` markers over subject and full body (Q100, Q102), plus the
   reindex-until-clean drain (Q14).
4. **JSONB-driven address logic**: containment over arrays of objects with GIN support (Q93/Q96, Q74), the account
   sidebar aggregation (union of four arrays unnested -> DISTINCT -> GROUP BY with count / conditional sums / max, suffix
   `ILIKE` filter, ordered by an aggregate) executed on every IMAP LIST/SELECT/STATUS (Q101), EXISTS-over-unnest with an
   array parameter (Q97), `?|` on reference lists, negative JSON indexing, and Postgres' canonical JSONB output form.
5. **Threading by normalised subject**: a regexp/lower/trim/md5 expression that must be identical in the query, in a
   partial expression index, and in the app's own normaliser; combined with link-following over `message_id` /
   `in_reply_to` / `reference_ids` (Q105, Q106).
6. **INBOX EXPUNGE ownership rule**: correlated EXISTS / NOT EXISTS over mapping rows with `LIKE` prefix/suffix tests
   (Q88).
7. **Keyset pagination and delta sync on DB time**: row-value comparison on `(timestamptz µs, uuid)` with a text
   round-trip cursor, mixed with OFFSET/LIMIT lists, plus a server-clock watermark (`now() − 2 s`) and `updated >`
   polling that assumes every mutation is stamped by the same clock (Q93–Q99).
8. **Large values**: range reads of multi-MB text by character offset and by UTF-8 byte offset, length-in-bytes and
   400-char prefix without loading the value, NUL-free text invariant (Q76, Q82, Q83, `{H}`).
9. **Schema lifecycle**: hash-gated idempotent DDL, additive auto-migration by catalog introspection under an advisory
   lock inside a DDL transaction, concurrent index builds with invalid-index sweep, a cancellable long-running
   maintenance session with its own timeout, two engines sharing one schema, and FK cascades.
10. **Operational semantics**: per-connection 30 s statement timeout with out-of-band cancel, text-typed parameters with
    server-side inference/casts, error-absorbing reads vs. must-propagate reads, and result typing conventions
    (bigint -> number, timestamptz -> ms ISO string) that both servers and their golden tests encode.
