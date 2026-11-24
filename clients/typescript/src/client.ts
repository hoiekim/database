// The client: statements, transactions, named locks, administration.

import type * as ast from "./ast.js";
import { ColumnDef, Delete, IndexDef, Insert, Query, TableDef, Update, type Arg } from "./builders.js";
import { type Config, type Conn, type Lease, Pool, configFromUrl } from "./conn.js";
import { DbError, ErrorCode } from "./errors.js";
import { type ReadValue, Timestamp, fromWire } from "./values.js";

/** How many times `Client.transaction` runs its work beside other writers before it runs it as the only one. */
const OPTIMISTIC_RUNS = 2;

function unexpected(what: string): DbError {
  return new DbError(ErrorCode.Connection, `the server answered ${what} with something else`);
}

function count(n: bigint): number {
  if (n > BigInt(Number.MAX_SAFE_INTEGER)) throw new DbError(ErrorCode.Connection, "a count beyond what a number can hold");
  return Number(n);
}

/** One row of a result: its values in order, also read by column name. */
export class Row {
  constructor(
    readonly columns: readonly string[],
    readonly values: readonly ReadValue[],
  ) {}

  /** The value of the named column. Throws if the result has no such column. */
  get(name: string): ReadValue {
    const at = this.columns.indexOf(name);
    if (at < 0) throw new DbError(ErrorCode.NotFound, `no column ${JSON.stringify(name)} in the result`);
    return this.values[at]!;
  }

  /** The value at a position, counting from 0. */
  at(index: number): ReadValue {
    if (index < 0 || index >= this.values.length) throw new DbError(ErrorCode.NotFound, `no column ${index} in the result`);
    return this.values[index]!;
  }

  /** An int column as a number. Throws if it is not an int, or one too large for a number to hold exactly. */
  int(name: string): number {
    const v = this.get(name);
    if (typeof v !== "number" || !Number.isSafeInteger(v)) throw new DbError(ErrorCode.TypeMismatch, `column ${JSON.stringify(name)} is not an int a number can hold`);
    return v;
  }

  /** An int column as a bigint, whatever its size. */
  bigint(name: string): bigint {
    const v = this.get(name);
    if (typeof v === "bigint") return v;
    if (typeof v === "number" && Number.isSafeInteger(v)) return BigInt(v);
    throw new DbError(ErrorCode.TypeMismatch, `column ${JSON.stringify(name)} is not an int`);
  }

  toObject(): Record<string, ReadValue> {
    return Object.fromEntries(this.columns.map((name, i) => [name, this.values[i]!]));
  }
}

/** The rows a statement returned, and how many rows it touched. */
export class Rows implements Iterable<Row> {
  readonly columns: readonly string[];
  readonly rows: readonly Row[];
  /** Rows a write inserted, updated or deleted, whether or not it returned them; rows a read returned. */
  readonly affected: number;
  /** For a query built `withTotal`: how many rows there are before offset and limit. */
  readonly total: number | null;

  constructor(set: ast.RowSet) {
    this.columns = set.columns;
    this.rows = set.rows.map((row) => new Row(set.columns, row.map(fromWire)));
    this.affected = count(set.affected);
    this.total = set.total === null ? null : count(set.total);
  }

  get length(): number {
    return this.rows.length;
  }

  [Symbol.iterator](): Iterator<Row> {
    return this.rows[Symbol.iterator]();
  }

  /** The first row, if there is one. */
  opt(): Row | null {
    return this.rows[0] ?? null;
  }

  /** The only row. An error if there is none or more than one. */
  one(): Row {
    if (this.rows.length === 1) return this.rows[0]!;
    if (this.rows.length === 0) throw new DbError(ErrorCode.NotFound, "expected one row, got none");
    throw new DbError(ErrorCode.InvalidRequest, "expected one row, got several");
  }

  /** One column of every row: the first, or the one named. */
  column(name?: string): ReadValue[] {
    return this.rows.map((row) => (name === undefined ? row.at(0) : row.get(name)));
  }

  objects(): Record<string, ReadValue>[] {
    return this.rows.map((row) => row.toObject());
  }
}

function rows(response: ast.Response): Rows {
  if (response.kind !== "rows") throw unexpected("a statement");
  return new Rows(response);
}

function ok(response: ast.Response): void {
  if (response.kind !== "ok") throw unexpected("a command");
}

function spec<S>(statement: { spec: S } | S): S {
  return statement instanceof Query ||
    statement instanceof Insert ||
    statement instanceof Update ||
    statement instanceof Delete ||
    statement instanceof TableDef ||
    statement instanceof ColumnDef ||
    statement instanceof IndexDef
    ? (statement.spec as S)
    : (statement as S);
}

/**
 * What a `Client` and a `Transaction` have in common, so code that reads
 * and writes can be handed either.
 *
 * On a `Client` each statement is its own transaction: it takes effect
 * whole or not at all, and is durable when the call returns.
 */
export abstract class Db {
  /** Sends one request and returns its answer. The other methods are built on this. */
  abstract call(request: ast.Request): Promise<ast.Response>;

  /** Runs a query. */
  async query(query: Query | ast.QuerySpec): Promise<Rows> {
    return rows(await this.call({ kind: "query", query: spec(query) }));
  }

  /** Runs a query and gives its first row, if it has one. */
  async queryOpt(query: Query | ast.QuerySpec): Promise<Row | null> {
    return (await this.query(query)).opt();
  }

  /** Runs a query that must give exactly one row. */
  async queryOne(query: Query | ast.QuerySpec): Promise<Row> {
    return (await this.query(query)).one();
  }

  /** The row with this primary key. */
  get(table: string, key: Iterable<Arg>): Promise<Row | null> {
    return this.queryOpt(Query.table(table).key(key));
  }

  /**
   * Inserts rows. The result holds what `returning` asked for and, as
   * `affected`, how many rows were inserted or updated.
   */
  async insert(insert: Insert | ast.InsertSpec): Promise<Rows> {
    return rows(await this.call({ kind: "insert", insert: spec(insert) }));
  }

  async update(update: Update | ast.UpdateSpec): Promise<Rows> {
    return rows(await this.call({ kind: "update", update: spec(update) }));
  }

  async delete(del: Delete | ast.DeleteSpec): Promise<Rows> {
    return rows(await this.call({ kind: "delete", delete: spec(del) }));
  }

  private async ddl(ddl: ast.Ddl): Promise<void> {
    ok(await this.call({ kind: "ddl", ddl }));
  }

  /** Creates a table. An error if one of that name exists. */
  createTable(def: TableDef | ast.TableSpec): Promise<void> {
    return this.ddl({ kind: "create_table", def: spec(def), ifNotExists: false });
  }

  /** Creates a table unless one of that name exists. Does not compare the definitions. */
  createTableIfNotExists(def: TableDef | ast.TableSpec): Promise<void> {
    return this.ddl({ kind: "create_table", def: spec(def), ifNotExists: true });
  }

  dropTable(name: string): Promise<void> {
    return this.ddl({ kind: "drop_table", name, ifExists: false });
  }

  dropTableIfExists(name: string): Promise<void> {
    return this.ddl({ kind: "drop_table", name, ifExists: true });
  }

  /** Adds a column. Existing rows read its default, or NULL. */
  addColumn(table: string, column: ColumnDef | ast.ColumnSpec): Promise<void> {
    return this.ddl({ kind: "add_column", table, column: spec(column), ifNotExists: false });
  }

  addColumnIfNotExists(table: string, column: ColumnDef | ast.ColumnSpec): Promise<void> {
    return this.ddl({ kind: "add_column", table, column: spec(column), ifNotExists: true });
  }

  dropColumn(table: string, column: string): Promise<void> {
    return this.ddl({ kind: "drop_column", table, column, ifExists: false });
  }

  dropColumnIfExists(table: string, column: string): Promise<void> {
    return this.ddl({ kind: "drop_column", table, column, ifExists: true });
  }

  /** Adds an index and fills it from the rows already there. */
  createIndex(table: string, index: IndexDef | ast.IndexSpec): Promise<void> {
    return this.ddl({ kind: "create_index", table, index: spec(index), ifNotExists: false });
  }

  createIndexIfNotExists(table: string, index: IndexDef | ast.IndexSpec): Promise<void> {
    return this.ddl({ kind: "create_index", table, index: spec(index), ifNotExists: true });
  }

  dropIndex(table: string, index: string): Promise<void> {
    return this.ddl({ kind: "drop_index", table, index, ifExists: false });
  }

  dropIndexIfExists(table: string, index: string): Promise<void> {
    return this.ddl({ kind: "drop_index", table, index, ifExists: true });
  }

  /** Every table's definition as the server holds it. */
  async describe(): Promise<ast.TableSpec[]> {
    const response = await this.call({ kind: "describe" });
    if (response.kind !== "tables") throw unexpected("describe");
    return response.tables;
  }

  /** One table's definition, if the table exists. */
  async describeTable(name: string): Promise<ast.TableSpec | null> {
    return (await this.describe()).find((t) => t.name === name) ?? null;
  }

  /** The server's clock. Inside a transaction, the moment it began. */
  async now(): Promise<Timestamp> {
    const response = await this.call({ kind: "now" });
    if (response.kind !== "time") throw unexpected("now");
    return new Timestamp(response.micros);
  }
}

function ended(): DbError {
  return new DbError(ErrorCode.TransactionAborted, "the transaction has already ended");
}

/**
 * An open transaction. Statements run through it see each other's
 * effects; nobody else sees them until `commit`. It occupies one pooled
 * connection until it is committed or rolled back: end every transaction
 * that is begun, or use `Client.transaction`, which does.
 *
 * After a statement fails, the transaction can only be rolled back, or
 * rolled back to a savepoint taken before the failure.
 */
export class Transaction extends Db {
  private lease: Lease | null;
  private queue: Promise<unknown> = Promise.resolve();
  private didConflict = false;

  constructor(lease: Lease) {
    super();
    this.lease = lease;
  }

  /**
   * Whether the server has ended the transaction because a commit made
   * beside it changed something it had read: it can be run again. Only
   * an optimistic transaction ever is.
   */
  get conflicted(): boolean {
    return this.didConflict;
  }

  get ended(): boolean {
    return this.lease === null;
  }

  /** One request at a time, in the order they were made. */
  private inTurn<T>(work: () => Promise<T>): Promise<T> {
    const result = this.queue.then(work);
    this.queue = result.catch(() => undefined);
    return result;
  }

  private note(e: unknown): never {
    if (e instanceof DbError && e.code === ErrorCode.Conflict) this.didConflict = true;
    throw e;
  }

  call(request: ast.Request): Promise<ast.Response> {
    return this.inTurn(async () => {
      if (!this.lease) throw ended();
      return this.lease.conn.call(request).catch((e) => this.note(e));
    });
  }

  private finish(kind: "commit" | "rollback"): Promise<void> {
    return this.inTurn(async () => {
      const lease = this.lease;
      if (!lease) throw ended();
      this.lease = null;
      try {
        // Whatever the answer, the server has ended the transaction: an
        // error on commit means it was rolled back.
        ok(await lease.conn.call({ kind }).catch((e) => this.note(e)));
      } finally {
        lease.held = false;
        lease.release();
      }
    });
  }

  /** Makes the transaction's changes permanent and visible. When this resolves they are on disk. */
  commit(): Promise<void> {
    return this.finish("commit");
  }

  rollback(): Promise<void> {
    return this.finish("rollback");
  }

  /** Marks a point to return to with `rollbackTo`. */
  async savepoint(name: string): Promise<void> {
    ok(await this.call({ kind: "savepoint", name }));
  }

  /**
   * Undoes everything since the savepoint, which stays in place. Also
   * clears the failed state a statement error left behind.
   */
  async rollbackTo(name: string): Promise<void> {
    ok(await this.call({ kind: "rollback_to", name }));
  }

  /** Forgets the savepoint, keeping what was done since. */
  async release(name: string): Promise<void> {
    ok(await this.call({ kind: "release", name }));
  }

  /** Rolls back if the transaction is still open, and never throws. */
  async abandon(): Promise<void> {
    if (!this.ended) await this.rollback().catch(() => undefined);
  }
}

/** A named lock held on the server. See `Client.lock`. */
export class LockGuard {
  private lease: Lease | null;

  constructor(
    lease: Lease,
    readonly name: string,
  ) {
    this.lease = lease;
  }

  /** Gives the lock up. */
  async unlock(): Promise<void> {
    const lease = this.lease;
    this.lease = null;
    if (!lease) return;
    try {
      ok(await lease.conn.call({ kind: "unlock", name: this.name }));
      lease.held = false;
    } finally {
      // Still held, the connection is closed, which gives the lock up too.
      lease.release();
    }
  }
}

/** A handle on the database. It keeps a small pool of connections and opens them as they are needed. */
export class Client extends Db {
  private readonly pool: Pool;

  private constructor(readonly config: Config) {
    super();
    this.pool = new Pool(config);
  }

  /**
   * Connects to a `database://` URL or a `Config`, checking the address
   * and the password with one connection.
   */
  static async connect(target: string | Config): Promise<Client> {
    const client = new Client(typeof target === "string" ? configFromUrl(target) : target);
    await client.ping();
    return client;
  }

  call(request: ast.Request): Promise<ast.Response> {
    return this.pool.with((conn) => conn.call(request));
  }

  /** Whether the server answers. */
  async ping(): Promise<void> {
    ok(await this.call({ kind: "ping" }));
  }

  /**
   * Closes the idle connections now, and the ones in use as they are
   * released. The client can still be used and will reconnect.
   */
  close(): void {
    this.pool.clear();
  }

  // ---- transactions

  private async beginWith(txn: ast.TxnKind): Promise<Transaction> {
    const lease = await this.pool.acquire();
    try {
      ok(await lease.conn.call({ kind: "begin", txn, lockWaitMs: BigInt(Math.trunc(this.config.lockWait * 1000)) }));
    } catch (e) {
      lease.release();
      throw e;
    }
    lease.held = true;
    return new Transaction(lease);
  }

  /**
   * Starts a transaction that is the database's only writer until it
   * ends. It waits for any other writer to finish, and every other
   * writer then waits for it: keep it short. Its commit cannot conflict.
   */
  begin(): Promise<Transaction> {
    return this.beginWith("exclusive");
  }

  /**
   * Starts a transaction that may write and holds nobody up: it runs
   * beside other writers, on a copy of the database that only it sees.
   * Its `commit` checks that no commit made meanwhile has changed
   * anything it was told; if one has, the commit fails with
   * `ErrorCode.Conflict`, nothing of the transaction takes effect, and
   * it can be run again. `transaction` does all of that.
   */
  beginOptimistic(): Promise<Transaction> {
    return this.beginWith("optimistic");
  }

  /**
   * Starts a transaction that only reads. It sees the database as of
   * this moment whatever is written meanwhile, and blocks nobody.
   */
  beginRead(): Promise<Transaction> {
    return this.beginWith("read");
  }

  /**
   * Runs `work(tx)` in a writing transaction: commits if it resolves,
   * rolls back if it throws. The result is as if nothing else had
   * written while it ran.
   *
   * `work` may be called more than once. It runs first beside other
   * writers (see `beginOptimistic`). If another writer's commit then
   * proves to have changed something it read, that run is undone whole
   * and `work` is called again: once more the same way, and after that
   * as the database's only writer, which cannot fail so. `work` should
   * therefore do nothing but talk to the database through the
   * transaction it is given; use `exclusive` for work that must run
   * exactly once.
   */
  async transaction<T>(work: (tx: Transaction) => Promise<T>): Promise<T> {
    for (let run = 0; run < OPTIMISTIC_RUNS; run++) {
      const tx = await this.beginOptimistic();
      let value: T;
      try {
        value = await work(tx);
      } catch (e) {
        const conflicted = tx.conflicted;
        await tx.abandon();
        if (conflicted) continue;
        throw e;
      }
      try {
        await tx.commit();
        return value;
      } catch (e) {
        if (!(e instanceof DbError && e.code === ErrorCode.Conflict)) throw e;
      }
    }
    return this.exclusive(work);
  }

  /**
   * Runs `work(tx)`, once, in a transaction that is the database's only
   * writer while it runs (see `begin`).
   */
  async exclusive<T>(work: (tx: Transaction) => Promise<T>): Promise<T> {
    const tx = await this.begin();
    let value: T;
    try {
      value = await work(tx);
    } catch (e) {
      await tx.abandon();
      throw e;
    }
    await tx.commit();
    return value;
  }

  /** The same for a transaction that only reads: `work` sees one moment of the database. */
  async read<T>(work: (tx: Transaction) => Promise<T>): Promise<T> {
    const tx = await this.beginRead();
    try {
      return await work(tx);
    } finally {
      await tx.abandon();
    }
  }

  // ---- named locks

  /**
   * Takes the named lock, waiting as long as it takes. The lock belongs
   * to the guard, not to a transaction: it is held until `unlock`. For
   * work that only one process should do at a time, such as migrations.
   */
  async lock(name: string): Promise<LockGuard> {
    for (;;) {
      // Wait in slices, so a statement timeout cannot cut the wait.
      const guard = await this.lockFor(name, 5_000n);
      if (guard) return guard;
    }
  }

  /** Takes the named lock if nobody holds it. */
  tryLock(name: string): Promise<LockGuard | null> {
    return this.lockFor(name, 0n);
  }

  private async lockFor(name: string, waitMs: bigint): Promise<LockGuard | null> {
    const lease = await this.pool.acquire();
    try {
      const response = await lease.conn.call({ kind: "lock", name, waitMs });
      if (response.kind !== "flag") throw unexpected("lock");
      if (response.value) {
        lease.held = true;
        return new LockGuard(lease, name);
      }
    } catch (e) {
      lease.release();
      throw e;
    }
    lease.release();
    return null;
  }

  /** Server-wide operations. Most need the administrator's password. */
  get admin(): Admin {
    return new Admin(this.pool);
  }
}

/** Server-wide operations. See `Client.admin`. */
export class Admin {
  constructor(private readonly pool: Pool) {}

  private call(admin: ast.AdminRequest, conn?: Conn): Promise<ast.Response> {
    const request: ast.Request = { kind: "admin", admin };
    return conn ? conn.call(request) : this.pool.with((conn) => conn.call(request));
  }

  async createDatabase(name: string, password: string): Promise<void> {
    ok(await this.call({ kind: "create_database", name, password, ifNotExists: false }));
  }

  async createDatabaseIfNotExists(name: string, password: string): Promise<void> {
    ok(await this.call({ kind: "create_database", name, password, ifNotExists: true }));
  }

  /** Removes a database and its files. */
  async dropDatabase(name: string): Promise<void> {
    ok(await this.call({ kind: "drop_database", name, ifExists: false }));
  }

  async dropDatabaseIfExists(name: string): Promise<void> {
    ok(await this.call({ kind: "drop_database", name, ifExists: true }));
  }

  async setPassword(name: string, password: string): Promise<void> {
    ok(await this.call({ kind: "set_password", name, password }));
  }

  async listDatabases(): Promise<string[]> {
    const response = await this.call({ kind: "list_databases" });
    if (response.kind !== "names") throw unexpected("list databases");
    return response.names;
  }

  /** Moves everything the log holds into the data file. */
  async checkpoint(): Promise<void> {
    ok(await this.call({ kind: "checkpoint" }));
  }

  /**
   * Verifies the files of the connection's database, or of every
   * database on an administering connection. Returns a report; throws if
   * anything is wrong.
   */
  async check(): Promise<string> {
    const response = await this.call({ kind: "check" });
    if (response.kind !== "text") throw unexpected("check");
    return response.text;
  }

  async stats(): Promise<string> {
    const response = await this.call({ kind: "stats" });
    if (response.kind !== "text") throw unexpected("stats");
    return response.text;
  }

  /**
   * Hands a dump of the named databases, or of all of them when none is
   * named, to `write`, piece by piece. The dump is each database as of
   * one moment, taken without stopping writers. Returns the bytes written.
   */
  dump(write: (chunk: Uint8Array) => void | Promise<void>, databases: readonly string[] = []): Promise<number> {
    return this.pool.with(async (conn) => {
      await conn.send({ kind: "admin", admin: { kind: "dump", databases: [...databases] } });
      let written = 0;
      for (;;) {
        const response = await conn.receive();
        if (response.kind === "chunk") {
          await write(response.bytes);
          written += response.bytes.length;
        } else if (response.kind === "ok") {
          conn.settle();
          return written;
        } else if (response.kind === "error") {
          conn.settle();
          throw response.error;
        } else {
          throw unexpected("dump");
        }
      }
    });
  }

  /**
   * Loads a dump from `source`: its bytes, or pieces of it as they come
   * (a file's read stream is such a source). Each database in it is
   * created; one that already exists is an error unless `replace`, which
   * swaps it for the dump's. Nothing is changed unless the whole dump is
   * read and verified.
   */
  async restore(source: Uint8Array | Iterable<Uint8Array> | AsyncIterable<Uint8Array>, replace = false): Promise<void> {
    const pieces = source instanceof Uint8Array ? [source] : source;
    const lease = await this.pool.acquire();
    try {
      // Until the restore ends, the connection holds a half-loaded dump.
      lease.held = true;
      ok(await this.call({ kind: "restore_begin", replace }, lease.conn));
      for await (const piece of pieces) {
        for (let at = 0; at < piece.length; at += 1 << 20) {
          ok(await this.call({ kind: "restore_chunk", bytes: piece.subarray(at, at + (1 << 20)) }, lease.conn));
        }
      }
      try {
        ok(await this.call({ kind: "restore_end" }, lease.conn));
      } finally {
        lease.held = false;
      }
    } finally {
      lease.release();
    }
  }
}
