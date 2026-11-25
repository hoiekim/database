// A real server for the integration tests: the `database` binary on an
// empty data directory and a free port, stopped and removed afterwards.
//
// The binary is `$DATABASE_BIN` if that is set, and otherwise
// `target/debug/database` of this repository (`cargo build -p
// database-server`). With neither there, the tests that need a server
// are skipped; with `DATABASE_BIN` set to something that is not there,
// they fail, so that CI cannot pass by testing nothing.

import { type ChildProcess, execFile, spawn } from "node:child_process";
import { existsSync, mkdtempSync, openSync, readFileSync, rmSync } from "node:fs";
import * as net from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Client, type Config, config } from "../src/index.js";
import { REPOSITORY } from "./describe.js";

export const ADMIN_PASSWORD = "admin-password";
export const PASSWORD = "test-password";

export function binary(): string | null {
  const named = process.env.DATABASE_BIN;
  if (named) {
    if (!existsSync(named)) throw new Error(`DATABASE_BIN names ${named}, which is not there`);
    return named;
  }
  const built = join(REPOSITORY, "target", "debug", "database");
  return existsSync(built) ? built : null;
}

function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const probe = net.createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const port = (probe.address() as net.AddressInfo).port;
      probe.close(() => resolve(port));
    });
  });
}

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

export class Server {
  private child!: ChildProcess;

  private constructor(
    private readonly path: string,
    private readonly dir: string,
    readonly port: number,
  ) {}

  static async start(path: string): Promise<Server> {
    const server = new Server(path, mkdtempSync(join(tmpdir(), "database-typescript-test-")), await freePort());
    await server.run();
    return server;
  }

  private environment(): NodeJS.ProcessEnv {
    return { ...process.env, DATABASE_ADMIN_PASSWORD: ADMIN_PASSWORD, DATABASE_DATA: join(this.dir, "data"), DATABASE_LISTEN: `127.0.0.1:${this.port}` };
  }

  private async run(): Promise<void> {
    const log = openSync(join(this.dir, "server.log"), "a");
    this.child = spawn(this.path, ["serve"], {
      env: this.environment(),
      stdio: ["ignore", log, log],
    });
    const deadline = Date.now() + 60_000;
    for (;;) {
      try {
        (await this.admin()).close();
        return;
      } catch (e) {
        if (this.child.exitCode !== null || Date.now() > deadline) {
          const output = readFileSync(join(this.dir, "server.log"), "utf8");
          await this.stop();
          throw new Error(`the server did not start: ${String(e)}; its log:\n${output}`);
        }
        await sleep(50);
      }
    }
  }

  config(database = "", password = PASSWORD): Config {
    return config("127.0.0.1", this.port, database, database === "" ? ADMIN_PASSWORD : password);
  }

  /** A client with the administrator's password, on no database. */
  admin(): Promise<Client> {
    return Client.connect(this.config());
  }

  /** A client on the named database, which is created if it is not there yet. */
  async client(database: string, settings: Partial<Config> = {}): Promise<Client> {
    const admin = await this.admin();
    await admin.admin.createDatabaseIfNotExists(database, PASSWORD);
    admin.close();
    return Client.connect({ ...this.config(database), ...settings });
  }

  /**
   * Runs `database role <arguments>` against this server: this client
   * greets with a protocol version that has no request to make a role or
   * grant it anything. `password` is the new role's, for `create`.
   */
  role(arguments_: string[], password = ""): Promise<void> {
    return new Promise((resolve, reject) => {
      execFile(this.path, ["role", ...arguments_], { env: { ...this.environment(), DATABASE_PASSWORD: password }, timeout: 60_000 }, (e, _stdout, stderr) => {
        if (e) reject(new Error(`database role ${arguments_.join(" ")} failed: ${stderr.trim() || e.message}`));
        else resolve();
      });
    });
  }

  private async stopProcess(): Promise<void> {
    const child = this.child;
    if (child.exitCode !== null || child.signalCode !== null) return;
    const exited = new Promise<void>((resolve) => child.once("exit", () => resolve()));
    child.kill("SIGTERM");
    const killer = setTimeout(() => child.kill("SIGKILL"), 30_000);
    await exited;
    clearTimeout(killer);
  }

  /** Stops the server and starts it again on the same files and port: every connection made before is closed. */
  async restart(): Promise<void> {
    await this.stopProcess();
    await this.run();
  }

  async stop(): Promise<void> {
    await this.stopProcess();
    rmSync(this.dir, { recursive: true, force: true });
  }
}
