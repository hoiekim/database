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
database base [name...]              write a base of each database to the archive
database archive [name] [--commits]  verify the archive; with --commits, list every commit
database recover <name> [--to <moment>] [--as <name>] [--replace]
                                     restore a database from the archive as it stood at a moment
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
| `DATABASE_ARCHIVE` | (none) | a directory to keep the log in beyond its checkpoints (see "Restoring to a moment") |
| `DATABASE_ARCHIVE_SECONDS` | `60` | with an archive: the longest a commit waits before its log file is copied there; `0` leaves it to the ordinary checkpoints |

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

## Restoring to a moment

A dump is the database at the moment the dump was taken. To have it
back as it stood at any other moment (a minute before a table was
emptied by mistake, the last commit before a disk died), the server
keeps its log.

```sh
DATABASE_ARCHIVE=/archive database serve
```

`/archive` is a directory of the operator's choosing, best on another
disk than the data, or a mount of storage elsewhere. With it set:

- Each database's log files are **copied there before a checkpoint
  removes them**, each whole and synced before the original goes.
- Each database has a **base** there: a copy of its data file as of a
  checkpoint. The server writes one when a database is created, and
  when it finds an archive that holds none the log continues from (the
  first start with `DATABASE_ARCHIVE`, an archive that was emptied).
  `database base` writes another at any time, while the server runs: a
  restore starts from the newest base before its moment, so a newer one
  makes restores shorter, and lets what is older be removed.
- Every commit's record carries the time it was made.

A base and the log files after it, with none missing, give the database
at every commit from the base on.

```text
/archive/shop.5f0c…e1/base-0000000012   the data file as log file 12 begins
/archive/shop.5f0c…e1/wal-0000000012    log file 12
/archive/shop.5f0c…e1/wal-0000000013
```

A directory is one **history**: a database's name and the identity of
its data file. A database dropped and created again under its name is a
new history, and so is a database that was restored: what is committed
to it afterwards is not what was committed after that moment the first
time, and the two are never mixed.

### How far behind the archive is

A log file reaches the archive at the checkpoint that closes it. The
server checkpoints when the log passes 8 MiB, when a database has been
quiet for five seconds after a write, and, with an archive, once the
oldest commit not yet copied is `DATABASE_ARCHIVE_SECONDS` old (60 by
default). So the archive is at most about that far behind, and what it
lacks is in the data directory. `database checkpoint` brings it up to
the moment the command is given. A server that loses its data disk
loses the commits of that last interval from the archive too: closing
that gap is what a standby is for.

### Taking a database back

```sh
database archive shop --commits                  # what the archive holds, commit by commit
database recover shop --to 2026-11-25T12:03:07Z --as shop_then
database recover shop --to 14:8812 --as shop_then
database recover shop --replace                  # the end of the archive, over the database itself
```

`--to` is a time, a commit's position, or `end` (the default):

- **A time** (read as UTC unless it carries an offset): every commit
  made at or before it. A commit acknowledged by then is in; a commit
  first logged after it is not. A commit is stamped as it is logged,
  just before it is acknowledged, so the few that were on their way to
  the disk at that instant are in.
- **A position**, `file:byte`, as `database archive --commits` prints
  beside each commit's time and size: every commit up to that one.
  This is the handle for "everything before that one statement".
- **`end`**: every commit the archive holds.

`--as` names the database to make; without it the database is restored
under its own name. A database that exists is replaced only with
`--replace`, and keeps its password; a new one takes its password from
`DATABASE_PASSWORD`, as `create` does. `recover` names a database, or a
history by its directory's name when the archive holds several of one
name and the server has no database of that name now (a database that
was dropped, then created again and dropped again).

The restored database is built beside the others and takes its name
only when it is whole: a restore that fails, or a server that dies half
way, leaves everything as it was. It starts a history of its own, with
a base of its own, and the history it was restored from stays in the
archive, whole, to be restored from again.

`recover` **refuses**, and changes nothing, when it could not give
exactly what was asked for:

- a moment before the oldest base in the archive;
- a time later than the archive is known to be whole up to, which is
  when its last log file was closed: run `database checkpoint` on the
  running server first, or ask for `end`;
- a position the archive does not reach;
- a base that is damaged, or a log file on the way that is missing,
  damaged, or does not end where the next one says it ended;
- a database of the target's name that exists, without `--replace`;
- a server started without `DATABASE_ARCHIVE`.

### Checking the archive

`database archive` reads every base and every log file of every history
(or of the one named), verifies each base's checksum, each log record's
checksum, and that each log file ends where the next says it ended, and
prints what each history holds:

```text
shop.5f0c…e1: base 1, 16384 bytes, taken 2026-11-20T09:00:00.120000Z
shop.5f0c…e1: base 212, 4472832 bytes, taken 2026-11-24T02:00:03.551000Z
shop.5f0c…e1: whole: 2 bases, log files 1 to 367, 48211 commits, made from 2026-11-20T09:00:01.003211Z to 2026-11-25T12:07:44.918332Z, the last at 367:20931
```

It exits with an error at the first history that is not **whole**: one
whose oldest base cannot be followed, file by file, to its newest log
file. The message names the file. Any moment from a whole history's
oldest base to its last commit can be restored. `database stats` says
how the running server stands with the archive: the position of the
newest commit, the files written, and whether the archive is refusing
them. The proof that a backup restores is a restore: `recover --as
drill`, `database check`, `database drop drill`.

### When the archive cannot be written

A full archive, a mount that is gone, a directory without permission:
the server **keeps the log files in the data directory** and goes on.
No commit is refused and none waits. Each checkpoint, and a retry every
five seconds, offers the kept files again, in order; the server says so
once in its log, and in `stats`:

```text
database: archive: shop.5f0c…e1 of database "shop" is not in the archive, and its log files are kept until it is: i/o error: No space left on device (os error 28)
```

So every acknowledged commit is always in one of the two places. What
this costs is room on the data disk, without limit: an archive left
broken fills it, and a full data disk stops commits as it always does
("When the disk fails"). The two other things a server could do were
not chosen. Refusing commits while the archive is away would make the
archive a second thing that must be up for the database to be. Letting
the log go unarchived would leave a hole no later base closes for the
moments in it. The operator can choose that second one, knowingly:
start the server without `DATABASE_ARCHIVE` and the kept files are
removed; started with it again, each database begins with a new base,
and `database archive` reports the gap until the files before that base
are removed.

While the archive refuses a database's log, that database cannot be
dropped or restored over: its last commits would have no other copy.
A server started while its archive is away starts, and keeps the log.

### What to keep, and what it costs

The server never removes anything from the archive. To let go of old
moments, remove a history's files older than a base you keep: that
base and every `wal-` file numbered from it on are all a restore needs.
A history whose database is gone can be removed whole.

The archive takes the room of the log, which with an archive is nine
bytes longer for each commit (its time), and each base the size of its
data file. A commit costs the server the reading of a clock
and nine bytes of log; the copying is done at checkpoints, beside the
writers, and is two syncs on the archive for each log file, not for
each commit. A base is copied with checkpoints of that database held
off, so its log grows meanwhile; commits go on. Measured figures are in
`PERFORMANCE.md`. A server without `DATABASE_ARCHIVE` does none of
this, and its log is byte for byte what it was.

Several servers may keep their logs in one directory: a history is
named by its data file's identity as well as by its database's name.

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
<archive>/<name>.<id>/    with DATABASE_ARCHIVE: one history's bases and log files
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

With `DATABASE_ARCHIVE`, every record of the log begins with the time
of its commit. Builds from before the archive do not know that record
either, and refuse to start on a log that holds one: the same clean
stop empties the logs. Nothing else in the data directory differs, and
the archive directory is not read by a server started without it.

A database with a column that keeps the start of its large values in
its rows (`keep_start`, `CLIENT.md`) is stored in a form that a build
from before that setting cannot read. Such a build refuses to start on
the directory, and so serves none of its databases:

```text
database: cannot start: corrupt database: the stored catalog is damaged: malformed message: flag is neither 0 nor 1
```

It refuses a dump of such a database in the same way (`the dump cannot
be loaded: a record is malformed`) and restores none of it. It never
reads such a database wrongly. A database whose columns keep no start
is stored as before, and either build reads it. To go back to an older
build, give each such table a copy without the setting, with the newer
build, before taking the dump.

## Memory

The server's memory is the page cache plus a part in proportion to it:

| | With the default 32 MB |
|---|---|
| Page cache (`DATABASE_CACHE_MB`), shared by all databases | 32 MB |
| Committed pages waiting for their checkpoint, per busy database | up to 4 MB |
| Pages of a transaction in progress, per writer | up to 8 MB |
| Requests and results in flight | requests over 64 KiB: 4 MB of them decoded at once, and each connection's next request as sent while it waits; results: their size |

An idle server with nothing cached holds about 3 MB. Under the
benchmark's load (`bench/`) it holds 45 to 50 MB once the load has
passed, and at most 117 to 130 MB while four connections load megabyte
statements into a table with a full-text index (150 to 171 MB before
large requests shared a budget): the cache, the requests and their log
records in flight, and freed memory that the allocator has not yet
returned. Large requests take turns for 4 MB of memory to be decoded and
answered in, a request larger than that runs alone, a request waiting
for its turn is held as it was sent, and the memory each held is
returned to the system as it is answered (on Linux with the GNU C
library; elsewhere the budget alone bounds it). A container limit of 128
MB fits ordinary requests with the default cache, but not quite a bulk
load of megabyte statements from four connections, which went 2 MB over
it in one run of six: give that 160 MB, and 192 MB if rows of a megabyte
and more are common (the load above has rows of 4 kB, and a row is held
whole), and raise `DATABASE_CACHE_MB` only if `stats` shows many disk
reads for the working set. A transaction that runs beside other writers
keeps at most about a megabyte of its own before it takes the write
lock.

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
