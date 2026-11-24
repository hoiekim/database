# Running it

One binary, `database`, is the server and the tool that talks to it.

```text
database serve                       run the server
database ping                        exit 0 if the server answers
database create <name>               create a database; its password from DATABASE_PASSWORD
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
| `DATABASE_SOCKET` | (none) | a Unix domain socket to listen on as well (see "On the same host") |
| `DATABASE_CACHE_MB` | `32` | memory for cached pages (see "Memory") |
| `DATABASE_SLOW_MS` | (none) | log every statement that takes this many milliseconds or more (see "Checking") |

It recovers every database in the data directory, then listens. On
`SIGTERM` or `SIGINT` it stops taking connections, lets requests in
flight finish, writes a checkpoint and exits; the next start then has
nothing to replay. Killed outright (`SIGKILL`, power loss), it replays
its log on the next start: every commit a client was told about is
there, nothing else is.

Only one server can open a data directory: a second one fails at once
rather than share the files.

Every other command connects to the server on `DATABASE_HOST` (default
`127.0.0.1`), the port of `DATABASE_LISTEN`, or through
`DATABASE_SOCKET` if that is set, with `DATABASE_ADMIN_PASSWORD`. Run
inside the server's container they need nothing else.

## On the same host

A client on the server's own host can skip the network stack. Give the
server a path in `DATABASE_SOCKET` and it listens on a Unix domain
socket there as well as on TCP; the client names the same path:

```text
database://shop:<password>@localhost?socket=/run/database/database.sock
```

What that saves each request depends on the system's loopback: about
15 µs of a round trip where it was measured on macOS, about 2 µs on
Linux (`PERFORMANCE.md`). Between containers, mount one volume at the
socket's directory in both.

- The socket file is created with the server's umask, and a client
  needs write permission on it to connect: that decides which users of
  the host can reach the server at all. Whoever connects still gives a
  database's password, as on TCP.
- A file left at the path by a server that is gone is replaced. A path
  that another server is listening on, or that holds something other
  than a socket, stops the server from starting.
- The file is removed when the server stops.

## In a container

The `Dockerfile` at the root of this repository builds an image whose
command is `database serve` and whose health check is `database ping`.
The server runs in it as user 1000, so a directory mounted at `/data`
must be writable by that user (`chown 1000 ./data`): one that Docker
creates for a bind mount belongs to root. With Docker Compose:

```yaml
services:
  database:
    build: .
    environment:
      DATABASE_ADMIN_PASSWORD: ${DATABASE_ADMIN_PASSWORD}
    volumes:
      - ./data:/data
    # On the host's loopback only, if at all.
    ports:
      - "127.0.0.1:5454:5454"
    mem_limit: 192m

  shop:
    image: example/shop
    environment:
      DATABASE_URL: database://shop:${SHOP_DB_PASSWORD}@database:5454
    depends_on:
      database:
        condition: service_healthy
```

An application connects with a URL:

```text
database://shop:<password>@database:5454
```

`shop` is the database's name; the password is that database's. Options
go in the query string: `?pool=8&statement_timeout=60&lock_wait=30&connect_timeout=10&name=shop`,
and `socket=` and `optimistic_runs=`, which `CLIENT.md` describes.

Each application gets its own database and password, created once:

```sh
docker compose exec -e DATABASE_PASSWORD=<password> database database create --if-not-exists shop
```

The protocol is not encrypted and a database's password crosses the
network as it is. Keep the server on a private network and do not
publish the port beyond loopback.

A rolling deploy runs two copies of an application against one database
for a moment. Both work: the server takes any number of connections,
writers take turns, and a migration lock (`db.lock("shop.migrate")`)
keeps the two from migrating at once.

## Backup

`database dump` writes every database to stdout as one stream: each
database's name and password hash, each table's definition and rows. It
reads one moment of each database however long it takes, and holds up no
writer. It takes the place of `pg_dumpall` in a backup job:

```sh
docker compose exec -T database database dump | gzip | age -r <recipient> > database.dump.gz.age
```

(or whatever compression, encryption and destination the job already
uses). The stream is binary, not SQL, and compresses well. Every record
in it carries a checksum and the stream ends with a count of what it
held.

## Restore

```sh
age -d -i key.txt database.dump.gz.age | gunzip | docker compose exec -T database database restore
```

`restore` refuses to touch a database that already exists, unless given
`--replace`. It builds every database of the dump under a temporary name
and swaps them in only when the whole stream has been read and checked:
a dump that is cut short or damaged, or a server that dies half way,
leaves the server as it was. Passwords come back with the databases, so
applications connect as before.

A restore drill, on any machine with Docker:

```sh
docker build -t database .
docker run -d --name drill -e DATABASE_ADMIN_PASSWORD=drill database
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
  size, and counts of commits and disk reads and writes; then how the
  commits of optimistic transactions went: as they stood, run again
  beside another's commit, or in conflict (and so run again by their
  client); then the requests received since the server started. Many
  conflicts mean transactions that read and write the same rows: see
  `PERFORMANCE.md`.
- `database checkpoint`: writes everything committed into the data
  files now and empties the logs. Never needed; useful before copying
  the files of a stopped server.
- **Slow statements.** With `DATABASE_SLOW_MS=100` the server writes a
  line to its log (standard error) for every statement that takes 100
  milliseconds or more:

  ```text
  database: slow: 153 ms, database "shop", client "shop-web": query "mails" by the whole table, filtered, limit 50: read 0 index entries and 98112 rows
  ```

  How long it took, on which database, for which client (the `name` of
  its URL), what the statement was, and how much it read to answer. The
  statement is described by its table, its way in and its clauses,
  never by its values. Names are written quoted and escaped, as the
  client sent them, so each line of the log is one line. For a statement on its own and for a batch the
  time includes the wait for the write lock and for the disk. In a
  transaction those waits are the `begin`'s and the `commit`'s, which
  are timed and logged as statements are: a `begin` line is a wait for
  the write lock, and a `commit` line a wait for the disk, or for an
  optimistic transaction's turn and its statements run again (it then
  counts what that second run read). A transaction's first statement,
  when it brings the begin with it, includes the begin's wait. There is no planner whose choice could be at
  fault: a statement is slow because of what its own access path made
  the server read, and the two counts say which it was. This one read
  the whole table to return 50 rows, and wants an index range. The
  same counts for any query, slow or not, are `db.explain(query)`
  (`CLIENT.md`).

## When the disk fails

A commit is answered only once it is on disk, so a disk that fails
costs availability and not commits.

- **A full disk, or a write or a sync that fails.** The request that
  met it fails with an I/O error. If it was a commit or a checkpoint,
  the database then answers `Unavailable` to every write, and to
  `check`, until the server is restarted: from inside, the server cannot
  tell how much of what failed reached the disk, and it does not guess.
  Reads go on and show every commit that was acknowledged. Make room or
  replace the disk, restart, and the server recovers from its files with
  everything acknowledged in place. A commit that was answered with the
  error may be in place too, as after a connection lost at the wrong
  moment.
- **A start while the disk is still full** may fail, since recovery
  writes a checkpoint and begins a log file. Nothing is harmed by
  trying. Start it again when there is room.
- **A server killed at any moment**, in a commit, a checkpoint, a
  `create`, a `drop`, a `restore` or its own recovery, starts again with
  every acknowledged commit and no part of anything else. A database
  that was being created, dropped or restored over is there whole, as it
  was before or as it was to be.
- **A disk that reports a sync it has not done** (a write cache that
  acknowledges a flush and loses power) can lose acknowledged commits,
  and no server can prevent it. What the server does is notice. A log
  that lost records gives the database as it stood at an earlier commit;
  if commits were logged after the lost ones, the server refuses to
  start and names the damaged log file. A data file that lost pages is
  noticed where a page fails its checksum, where its log no longer
  applies to it (the server refuses to start and says so), or where
  `check` finds the tables and indexes at odds, and not always: where a
  page was never written,
  the older page in its place is whole. Use disks that honour flushes,
  and run `database check` after a power loss on hardware that may not.

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

A file named `<name>--creating.db` or `<name>--restoring.*` is a
database that was being made when the server stopped, and the next start
removes it. A log file without its data file is what a `create` or a
`drop` left when the server was killed in the middle of it: it is
ignored, and removed when the name is used again.

Before going back to an earlier build of the server, stop this one in
good order (`SIGTERM`), so that its logs hold no commits. A log that
holds commits may begin with a record that says where the log file
before it ended. Builds from before that record do not know it, and
refuse to start on such a log rather than read past it.

## Memory

The server's memory is the page cache plus a part in proportion to it:

| | With the default 32 MB |
|---|---|
| Page cache (`DATABASE_CACHE_MB`), shared by all databases | 32 MB |
| Committed pages waiting for their checkpoint, per busy database | up to 4 MB |
| Pages of a transaction in progress, per writer | up to 8 MB |
| Requests and results in flight | requests over 64 KiB: 4 MB of them at once, as sent; results: their size |

An idle server with nothing cached holds about 3 MB. Under the
benchmark's load (`bench/`) it holds 45 to 50 MB once the load has
passed, and at most 112 to 125 MB while four connections load megabyte
statements into a table with a full-text index (150 to 171 MB before
large requests shared a budget): the cache, the requests and their log
records in flight, and freed memory that the allocator has not yet
returned. Large requests take turns for 4 MB of memory, a request larger
than that runs alone, and the memory each held is returned to the system
as it is answered (on Linux with the GNU C library; elsewhere the budget
alone bounds it). A container limit of 128 MB fits ordinary requests and
a bulk load of megabyte statements with the default cache; give it 192
MB if rows of a megabyte and more are common (the load above has rows of
4 kB, and a row is held whole), and raise `DATABASE_CACHE_MB` only if
`stats` shows many disk reads for the working set. A transaction that
runs beside other writers keeps at most about a megabyte of its own
before it takes the write lock.

The largest single request or result is 256 MiB, and it is held in
memory while it is handled. A text of tens of megabytes works; a habit
of selecting many such texts in one query will be felt.

## Migrating from Postgres

There is no tool that reads a Postgres dump: a schema is defined by the
application through the client (as `TableDef`s), so the application moves
its own data. The shape that works:

1. The new version of the application creates its tables with
   `create_table_if_not_exists` under a migration lock, as it did with
   SQL migrations.
2. A one-off command reads each table from Postgres and inserts the rows
   through the client, a few hundred per statement inside one
   transaction per table; `auto_increment` columns take the ids given
   and continue above them.
3. `database check`, compare counts, then point the application at the
   new URL.

Type mapping and SQL-to-API equivalents are in `CLIENT.md`.
