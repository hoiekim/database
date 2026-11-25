// Where the server is, one connection to it, and the pool that shares a few.

import * as net from "node:net";

import type * as ast from "./ast.js";
import { MAX_FRAME, PROTOCOL_VERSION, ProtocolError, decodeResponse, encodeRequest } from "./codec.js";
import { DbError, ErrorCode } from "./errors.js";

export const DEFAULT_PORT = 5454;

/** A pooled connection that sat idle this long is pinged before it is trusted with a request. */
const IDLE_CHECK_MS = 20_000;

/** The longest a timer waits: Node fires one set for longer at once. */
const LONGEST_TIMER_MS = 0x7fff_ffff;

/**
 * Bytes received and not yet read above which the socket stops reading,
 * so that a server sending faster than the caller takes (a dump handed to
 * a slow `write`) waits instead of filling this process's memory. A read
 * that needs a larger frame lets it read on.
 */
const HIGH_WATER = 1 << 20;

function lost(what: unknown): DbError {
  return new DbError(ErrorCode.Connection, `connection to the database lost: ${what instanceof Error ? what.message : String(what)}`);
}

/** Where the server is and how to talk to it. Times are in seconds. */
export interface Config {
  host: string;
  port: number;
  /** Empty for a connection that only administers the server. */
  database: string;
  /**
   * The role of the database to connect as, if any: `password` is then
   * the role's, and the client may do what the role was granted and
   * nothing else. Without it, or with `null`, the client connects with
   * the database's own password, which may do anything in it.
   */
  role?: string | null;
  password: string;
  /** Connections kept at most. A transaction or a held lock occupies one until it ends. */
  poolSize: number;
  connectTimeout: number;
  /** How long the server may spend on one statement. Zero for no limit beyond the server's own. */
  statementTimeout: number;
  /** How long a write waits for the database's write lock before it fails with `LockTimeout`. */
  lockWait: number;
  /** Sent to the server in the greeting. */
  name: string;
}

export function config(host: string, port = DEFAULT_PORT, database = "", password = ""): Config {
  return { host, port, database, password, poolSize: 8, connectTimeout: 10, statementTimeout: 60, lockWait: 30, name: "" };
}

/** What stands between a database's name and a role's in the greeting: `shop/reader`. */
const ROLE_SEPARATOR = "/";

/** What the greeting names: the database, and the role after it if the connection is a role's. */
function greetingDatabase(config: Config): string {
  return config.role == null ? config.database : `${config.database}${ROLE_SEPARATOR}${config.role}`;
}

function bad(why: string): DbError {
  return new DbError(ErrorCode.InvalidRequest, `invalid database URL: ${why}`);
}

function decode(text: string): string {
  if (/%(?![0-9a-fA-F]{2})/.test(text)) throw bad("a bad % escape");
  try {
    return decodeURIComponent(text);
  } catch {
    throw bad("not UTF-8");
  }
}

/**
 * Reads `database://<database>:<password>@<host>[:<port>][/<database>]`
 * with optional `?pool=8&connect_timeout=10&statement_timeout=60&lock_wait=30&name=app`
 * (times in seconds), and `&role=reader` to connect as a role of the
 * database, with the role's password in the password's place. The
 * database may be named before the password or as the path; an
 * administrator's URL names none (`database://<password>@<host>`), and
 * so can name no role. Percent-encoding is decoded in the
 * database, the password and the settings.
 */
export function configFromUrl(url: string): Config {
  const prefix = "database://";
  if (!url.startsWith(prefix)) throw bad("it must start with database://");
  const whole = url.slice(prefix.length);
  const mark = whole.indexOf("?");
  const rest = mark < 0 ? whole : whole.slice(0, mark);
  const query = mark < 0 ? "" : whole.slice(mark + 1);
  const at = rest.lastIndexOf("@");
  if (at < 0) throw bad("no password before the host");
  const userinfo = rest.slice(0, at);
  const address = rest.slice(at + 1);
  const colon = userinfo.indexOf(":");
  const user = colon < 0 ? "" : userinfo.slice(0, colon);
  const password = colon < 0 ? userinfo : userinfo.slice(colon + 1);
  const slash = address.indexOf("/");
  const hostport = slash < 0 ? address : address.slice(0, slash);
  const path = slash < 0 ? "" : address.slice(slash + 1);
  const portAt = hostport.lastIndexOf(":");
  let host = hostport;
  let port = DEFAULT_PORT;
  if (portAt >= 0) {
    host = hostport.slice(0, portAt);
    const text = hostport.slice(portAt + 1);
    if (!/^\d{1,5}$/.test(text) || Number(text) > 65535) throw bad("the port is not a number");
    port = Number(text);
  }
  if (host === "") throw bad("no host");
  const out = config(host, port, decode(path === "" ? user : path), decode(password));
  for (const pair of query.split("&")) {
    if (pair === "") continue;
    const eq = pair.indexOf("=");
    if (eq < 0) throw bad("a setting without a value");
    const key = pair.slice(0, eq);
    const value = decode(pair.slice(eq + 1));
    const seconds = (): number => {
      const s = /^[+]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/.test(value.trim()) ? Number(value) : NaN;
      if (!(s >= 0 && s < 2 ** 64)) throw bad("a time that is not a number of seconds");
      return s;
    };
    switch (key) {
      case "pool":
        if (!/^\d+$/.test(value) || Number(value) === 0) throw bad("pool must be a positive number");
        out.poolSize = Number(value);
        break;
      case "connect_timeout":
        out.connectTimeout = seconds();
        break;
      case "statement_timeout":
        out.statementTimeout = seconds();
        break;
      case "lock_wait":
        out.lockWait = seconds();
        break;
      case "name":
        out.name = value;
        break;
      case "role":
        if (value === "" || out.database === "") throw bad("role must be the name of a role, of a database the URL names");
        out.role = value;
        break;
      default:
        throw bad("an unknown setting");
    }
  }
  return out;
}

/** One connection: a socket that carries one request at a time. */
export class Conn {
  /**
   * Set while the stream cannot be trusted to be between two requests:
   * after an I/O error, a timeout, or a caller that went away before the
   * answer was read.
   */
  broken = false;
  lastUsed = Date.now();

  private readonly chunks: Buffer[] = [];
  private buffered = 0;
  private failure: DbError | null = null;
  private waiting: (() => void) | null = null;
  /** How many bytes the read under way waits to have. */
  private wanted = 0;
  private readonly statementTimeoutMs: number;
  /** The server enforces the statement's deadline; this outer limit only catches a server gone silent. */
  private readonly patienceMs: number;

  private constructor(
    private readonly socket: net.Socket,
    config: Config,
  ) {
    this.statementTimeoutMs = Math.trunc(config.statementTimeout * 1000);
    this.patienceMs = config.statementTimeout === 0 ? 86_400_000 : (config.statementTimeout + config.lockWait + 10) * 1000;
    socket.on("data", (chunk: Buffer) => {
      this.chunks.push(chunk);
      this.buffered += chunk.length;
      if (this.buffered >= Math.max(HIGH_WATER, this.wanted)) socket.pause();
      this.wake();
    });
    socket.on("error", (e) => this.fail(lost(e)));
    socket.on("close", () => this.fail(lost("the server closed the connection")));
  }

  static async open(config: Config): Promise<Conn> {
    const where = `${config.host}:${config.port}`;
    const socket = await new Promise<net.Socket>((resolve, reject) => {
      const socket = net.connect({ host: config.host.replace(/^\[|\]$/g, ""), port: config.port });
      const timer = setTimeout(() => {
        socket.destroy();
        reject(new DbError(ErrorCode.Connection, `timed out connecting to the database at ${where}`));
      }, Math.min(config.connectTimeout * 1000, LONGEST_TIMER_MS));
      socket.once("connect", () => {
        clearTimeout(timer);
        socket.removeAllListeners("error");
        resolve(socket);
      });
      socket.once("error", (e) => {
        clearTimeout(timer);
        reject(new DbError(ErrorCode.Connection, `cannot reach the database at ${where}: ${e.message}`));
      });
    });
    // Requests are small and each waits for its answer: never delay one
    // to batch it with the next.
    socket.setNoDelay(true);
    const conn = new Conn(socket, config);
    try {
      const hello: ast.Request = { kind: "hello", version: PROTOCOL_VERSION, database: greetingDatabase(config), password: config.password, client: config.name };
      const answer = await conn.exchange(hello, Date.now() + config.connectTimeout * 1000);
      if (answer.kind !== "hello") throw lost("unexpected answer to the greeting");
      return conn;
    } catch (e) {
      conn.close();
      throw e;
    }
  }

  close(): void {
    this.broken = true;
    this.socket.destroy();
  }

  /** Whether the connection, while it waits in the pool, keeps the process from ending. */
  keepsProcessAlive(keeps: boolean): void {
    if (keeps) this.socket.ref();
    else this.socket.unref();
  }

  private wake(): void {
    const waiting = this.waiting;
    this.waiting = null;
    waiting?.();
  }

  private fail(error: DbError): void {
    this.failure ??= error;
    this.broken = true;
    this.wake();
  }

  private async read(n: number, deadline: number): Promise<Uint8Array> {
    while (this.buffered < n) {
      if (this.failure) throw this.failure;
      const left = deadline - Date.now();
      if (left <= 0) throw lost("no answer in time");
      this.wanted = n;
      if (this.socket.isPaused()) this.socket.resume();
      await new Promise<void>((resolve) => {
        // A longer wait is taken in pieces.
        const timer = setTimeout(resolve, Math.min(left, LONGEST_TIMER_MS));
        this.waiting = () => {
          clearTimeout(timer);
          resolve();
        };
      });
      this.waiting = null;
    }
    const out = new Uint8Array(n);
    let filled = 0;
    while (filled < n) {
      const head = this.chunks[0]!;
      const take = Math.min(head.length, n - filled);
      out.set(head.subarray(0, take), filled);
      filled += take;
      if (take === head.length) this.chunks.shift();
      else this.chunks[0] = head.subarray(take);
    }
    this.buffered -= n;
    this.wanted = 0;
    if (this.buffered < HIGH_WATER && this.socket.isPaused()) this.socket.resume();
    return out;
  }

  async send(request: ast.Request): Promise<void> {
    const payload = encodeRequest(request, this.statementTimeoutMs);
    if (payload.length > MAX_FRAME) throw new DbError(ErrorCode.InvalidRequest, `a request of ${payload.length} bytes is larger than the ${MAX_FRAME} allowed`);
    // An earlier call that was abandoned midway left its answer unread:
    // the next answer read would be that one.
    if (this.broken) throw this.failure ?? lost("an earlier request on this connection was abandoned before its answer was read");
    this.broken = true;
    const frame = new Uint8Array(4 + payload.length);
    new DataView(frame.buffer).setUint32(0, payload.length, true);
    frame.set(payload, 4);
    await new Promise<void>((resolve, reject) => {
      this.socket.write(frame, (e) => (e ? reject(lost(e)) : resolve()));
    });
  }

  /**
   * Reads one frame. The connection stays marked broken until the caller
   * has read the last frame of the answer and calls `settle`.
   */
  async receive(deadline: number = Date.now() + this.patienceMs): Promise<ast.Response> {
    const head = await this.read(4, deadline);
    const length = new DataView(head.buffer).getUint32(0, true);
    if (length > MAX_FRAME) throw lost("the server sent a frame larger than allowed");
    const frame = await this.read(length, deadline);
    try {
      return decodeResponse(frame);
    } catch (e) {
      throw e instanceof ProtocolError ? lost(e) : e;
    }
  }

  settle(): void {
    this.broken = false;
    this.lastUsed = Date.now();
  }

  private async exchange(request: ast.Request, deadline: number): Promise<ast.Response> {
    await this.send(request);
    let merged: (ast.Response & { kind: "rows" }) | null = null;
    for (;;) {
      const response = await this.receive(deadline);
      if (response.kind === "rows") {
        // A result in several frames: the first frame's head, every frame's rows.
        if (merged) for (const row of response.rows) merged.rows.push(row);
        else merged = response;
        if (response.more) continue;
        this.settle();
        return { ...merged, more: false };
      }
      this.settle();
      if (response.kind === "error") throw response.error;
      return response;
    }
  }

  /**
   * Sends a request and reads its whole answer. An error from the server
   * is thrown and leaves the connection usable.
   */
  call(request: ast.Request): Promise<ast.Response> {
    return this.exchange(request, Date.now() + this.patienceMs);
  }
}

/**
 * A connection out of the pool. Released, it goes back, unless it is
 * broken or still holds a transaction or a lock, in which case it is
 * closed and the server undoes what it held.
 */
export class Lease {
  /** The server holds state for this connection: an open transaction or a named lock. */
  held = false;
  private released = false;

  constructor(
    private readonly pool: Pool,
    readonly conn: Conn,
    private readonly generation: number,
  ) {}

  release(): void {
    if (this.released) return;
    this.released = true;
    this.pool.giveBack(this.conn, !this.conn.broken && !this.held, this.generation);
  }
}

export class Pool {
  private idle: Conn[] = [];
  private free: number;
  /** Counts the calls of `clear`: a connection leased before one is closed when it comes back. */
  private generation = 0;
  private readonly waiters: (() => void)[] = [];

  constructor(readonly config: Config) {
    this.free = Math.max(1, config.poolSize);
  }

  async acquire(): Promise<Lease> {
    if (this.free > 0) this.free--;
    else await new Promise<void>((resolve) => this.waiters.push(resolve));
    try {
      for (;;) {
        const conn = this.idle.pop();
        if (!conn) {
          const generation = this.generation;
          return new Lease(this, await Conn.open(this.config), generation);
        }
        // The server closed it while it waited (a restart, say).
        if (conn.broken) {
          conn.close();
          continue;
        }
        conn.keepsProcessAlive(true);
        if (Date.now() - conn.lastUsed > IDLE_CHECK_MS) {
          try {
            await conn.call({ kind: "ping" });
          } catch {
            conn.close();
            continue;
          }
        }
        return new Lease(this, conn, this.generation);
      }
    } catch (e) {
      this.freeSlot();
      throw e;
    }
  }

  private freeSlot(): void {
    const next = this.waiters.shift();
    if (next) next();
    else this.free++;
  }

  giveBack(conn: Conn, keep: boolean, generation: number): void {
    if (keep && generation === this.generation) {
      // An idle connection does not keep the process from ending.
      conn.keepsProcessAlive(false);
      this.idle.push(conn);
    } else {
      conn.close();
    }
    this.freeSlot();
  }

  /** Runs `work` with a connection that goes back to the pool afterwards. */
  async with<T>(work: (conn: Conn) => Promise<T>): Promise<T> {
    const lease = await this.acquire();
    try {
      return await work(lease.conn);
    } finally {
      lease.release();
    }
  }

  /** Closes the idle connections. Ones in use close when released. */
  clear(): void {
    this.generation++;
    const idle = this.idle;
    this.idle = [];
    for (const conn of idle) conn.close();
  }
}
