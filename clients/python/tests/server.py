"""A real server for the integration tests: the `database` binary on an
empty data directory and a free port, stopped and removed afterwards.

The binary is ``$DATABASE_BIN`` if that is set, and otherwise
``target/debug/database`` of this repository (``cargo build -p
database-server``). With neither there, the tests that need a server are
skipped; with ``DATABASE_BIN`` set to something that is not there, they
fail, so that CI cannot pass by testing nothing.
"""

from __future__ import annotations

import os
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from typing import Dict, Optional

from database_client import Client, Config, DbError

ADMIN_PASSWORD = "admin-password"
PASSWORD = "test-password"

REPOSITORY = Path(__file__).resolve().parents[3]


def binary() -> Optional[Path]:
    named = os.environ.get("DATABASE_BIN")
    if named:
        path = Path(named)
        if not path.is_file():
            raise FileNotFoundError(f"DATABASE_BIN names {named}, which is not there")
        return path
    built = REPOSITORY / "target" / "debug" / "database"
    return built if built.is_file() else None


class Server:
    def __init__(self) -> None:
        path = binary()
        if path is None:
            raise unittest.SkipTest("no server binary: run `cargo build -p database-server`, or set DATABASE_BIN")
        self.path = path
        self.dir = tempfile.mkdtemp(prefix="database-python-test-")
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            self.port = probe.getsockname()[1]
        self.log = open(os.path.join(self.dir, "server.log"), "ab")
        self._start()

    def _environment(self) -> Dict[str, str]:
        return dict(os.environ, DATABASE_ADMIN_PASSWORD=ADMIN_PASSWORD, DATABASE_DATA=os.path.join(self.dir, "data"), DATABASE_LISTEN=f"127.0.0.1:{self.port}")

    def _start(self) -> None:
        self.process = subprocess.Popen([str(self.path), "serve"], env=self._environment(), stdout=self.log, stderr=self.log)
        deadline = time.monotonic() + 60
        while True:
            try:
                self.admin().close()
                break
            except DbError:
                if self.process.poll() is not None or time.monotonic() > deadline:
                    self.stop()
                    raise RuntimeError("the server did not start; its log:\n" + self.read_log()) from None
                time.sleep(0.05)

    def read_log(self) -> str:
        try:
            with open(os.path.join(self.dir, "server.log"), errors="replace") as f:
                return f.read()
        except OSError:
            return ""

    def config(self, database: str = "", password: str = PASSWORD) -> Config:
        return Config("127.0.0.1", self.port, database, password if database else ADMIN_PASSWORD)

    def admin(self) -> Client:
        """A client with the administrator's password, on no database."""
        return Client.connect(self.config())

    def client(self, database: str, **settings: float) -> Client:
        """A client on the named database, which is created if it is not
        there yet."""
        admin = self.admin()
        admin.admin.create_database_if_not_exists(database, PASSWORD)
        admin.close()
        config = self.config(database)
        for name, value in settings.items():
            setattr(config, name, value)
        return Client.connect(config)

    def role(self, *arguments: str, password: str = "") -> None:
        """Runs ``database role <arguments>`` against this server: this
        client greets with a protocol version that has no request to make
        a role or grant it anything. ``password`` is the new role's, for
        ``create``."""
        done = subprocess.run([str(self.path), "role", *arguments], env=dict(self._environment(), DATABASE_PASSWORD=password), capture_output=True, text=True, timeout=60)
        if done.returncode != 0:
            raise RuntimeError(f"database role {' '.join(arguments)} failed: {done.stderr.strip()}")

    def _stop_process(self) -> None:
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()

    def restart(self) -> None:
        """Stops the server and starts it again on the same files and port:
        every connection made before is closed."""
        self._stop_process()
        self._start()

    def stop(self) -> None:
        self._stop_process()
        self.log.close()
        shutil.rmtree(self.dir, ignore_errors=True)
