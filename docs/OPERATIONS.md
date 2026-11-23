# Running it

One binary, `database`, is the server and the tool that talks to it.

```text
database serve                       run the server
database ping                        exit 0 if the server answers
database create <name>               create a database; password from DATABASE_PASSWORD or --password
database drop <name>
database password <name>             change a database's password
database list
database dump [name...] > file       every database, or the named ones, to stdout
database restore [--replace] < file  load a dump from stdin
database check                       verify every database's files
database checkpoint
database stats
```

## The server

`database serve` reads its settings from the environment:

| Variable | Default | |
|---|---|---|
| `DATABASE_ADMIN_PASSWORD` | (required) | the password that may do anything: create and drop databases, dump, restore |
| `DATABASE_DATA` | `./data` | where the files live |
| `DATABASE_LISTEN` | `0.0.0.0:5454` | address to listen on |
| `DATABASE_CACHE_MB` | `32` | memory for cached pages (see "Memory") |

It recovers every database in the data directory, then listens. On
`SIGTERM` or `SIGINT` it stops taking connections, lets requests in
flight finish, writes a checkpoint and exits; the next start then has
nothing to replay. Killed outright (`SIGKILL`, power loss), it replays
its log on the next start: every commit a client was told about is
there, nothing else is.

Only one server can open a data directory: a second one fails at once
rather than share the files.

Every other command connects to the server on `DATABASE_HOST` (default
`127.0.0.1`), the port of `DATABASE_LISTEN`, with
`DATABASE_ADMIN_PASSWORD`. Run inside the server's container they need
nothing else.

## In the stack

The image is built by the `Dockerfile` at the root of this repository. A
service entry in `stack/services.ts` in place of `postgres`:

```ts
{
  name: "database",
  image: "hoie/database:latest",
  memory: "128M",
  cpus: "0.6",
  envFile: true,          // DATABASE_ADMIN_PASSWORD
  compose: {
    // On the host's loopback only, for an SSH tunnel.
    ports: ["127.0.0.1:5454:5454"],
    volumes: ["./.volumes/database:/data"],
    // The image's own health check is `database ping`.
  },
},
```

Apps wait for it as they waited for Postgres
(`depends_on: { database: { condition: "service_healthy" } }`) and
connect with a URL:

```text
database://mail:<password>@database:5454
```

`mail` is the database's name; the password is that database's. Options
go in the query string: `?pool=8&statement_timeout=60&lock_wait=30&connect_timeout=10&name=mail`.

Each app gets its own database and password, created once:

```sh
docker compose exec -e DATABASE_PASSWORD=<password> database database create --if-not-exists mail
```

The protocol is not encrypted and a database's password crosses the
compose network as it is, as with the Postgres it replaces. Do not
publish the port beyond loopback.

A rolling deploy runs two copies of an app against one database for a
moment. Both work: the server takes any number of connections, writers
take turns, and the migration lock (`db.lock("mail.migrate")`) keeps the
two from migrating at once.

## Backup

`database dump` writes every database to stdout as one stream: each
database's name and password hash, each table's definition and rows. It
reads one moment of each database however long it takes, and holds up no
writer. It is the `pg_dumpall` of `stack/backup/backup.ts`; the job there
becomes:

```ts
["database.dump.gz.enc", "docker", ["compose", "exec", "-T", "database", "database", "dump"], [0]],
```

The stream is binary (not SQL) and compresses well. Every record in it
carries a checksum and the stream ends with a count of what it held.

## Restore

```sh
bun backup/backup.ts --restore <backup>/database.dump.gz.enc > database.dump
docker compose exec -T database database restore < database.dump
```

`restore` refuses to touch a database that already exists, unless given
`--replace`. It builds every database of the dump under a temporary name
and swaps them in only when the whole stream has been read and checked:
a dump that is cut short or damaged, or a server that dies half way,
leaves the server as it was. Passwords come back with the databases, so
the apps connect as before.

A restore drill, on any machine with Docker:

```sh
docker run -d --name drill -e DATABASE_ADMIN_PASSWORD=drill hoie/database
docker exec -i drill database restore < database.dump
docker exec drill database list
docker exec drill database check
docker rm -f drill
```

## Checking

- `database ping`: the server answers. The container's health check.
- `database check`: reads every page of every database and verifies the
  page checksums, that every index holds exactly the entries its table's
  rows call for, that every large value belongs to one row, and that
  every page is either in use once or free. It holds writers off while
  it runs. Prints a line per table, or the first inconsistency.
- `database stats`: per database, its version, pages, free pages, log
  size, and counts of commits and disk reads and writes.
- `database checkpoint`: writes everything committed into the data
  files now and empties the logs. Never needed; useful before copying
  the files of a stopped server.

## Files

```text
<data>/LOCK               held by the running server
<data>/ACCESS             each database's salted password hash
<data>/<name>.db          the data, in 8 KiB pages
<data>/<name>.wal-<n>     commits since the last checkpoint
```

A copy of the directory taken while the server is **stopped** is a
complete backup. A copy taken while it runs is not guaranteed to be
(the data file and the log are read at different moments): use `dump`.

## Memory

The server's memory is the page cache plus a part in proportion to it:

| | With the default 32 MB |
|---|---|
| Page cache (`DATABASE_CACHE_MB`), shared by all databases | 32 MB |
| Committed pages waiting for their checkpoint, per busy database | up to 4 MB |
| Pages of a transaction in progress, per writer | up to 8 MB |
| Requests and results in flight | their size |

An idle server with nothing cached holds about 3 MB. Under the
benchmark's load (`bench/`) it moves between 60 and 110 MB: about 60 in
use, and the rest freed memory that the allocator has not yet returned
(the server asks it to every five seconds while requests arrive). A
container limit of 128 MB fits that load; give it 192 MB if requests or
rows of many megabytes are common, and raise `DATABASE_CACHE_MB` only if
`stats` shows many disk reads for the working set.

The largest single request or result is 256 MiB, and it is held in
memory while it is handled. A mail body of 35 MiB works; a habit of
selecting every body of a mailbox in one query will be felt.

## Migrating from Postgres

There is no tool that reads a Postgres dump: the schemas are defined by
the apps (in Rust, as `TableDef`s), and the apps' rewrite to this client
is where the data moves. The shape that works, per app:

1. The new app version creates its tables with `create_table_if_not_exists`
   under its migration lock, as it did with SQL migrations.
2. A one-off command of the app reads each table from Postgres and
   inserts the rows through the client, a few hundred per statement
   inside one transaction per table; `auto_increment` columns take the
   ids given and continue above them.
3. `database check`, compare counts, then point the app at the new URL.

Type mapping and SQL-to-API equivalents are in `CLIENT.md`.
