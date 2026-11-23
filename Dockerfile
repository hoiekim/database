# The database server and its command-line tool, in one binary.
#
#   docker build -t database .
#
# Configuration comes from the environment only (docs/OPERATIONS.md).

# The Rust version rust-toolchain.toml pins.
FROM rust:1.97.0-slim-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY protocol protocol
COPY engine engine
COPY client client
COPY server server
COPY bench bench

# --locked: the dependencies are exactly those of Cargo.lock.
RUN cargo build --release --locked -p database-server

FROM debian:bookworm-slim

# Owned by root: the server can read its binary but not change it.
COPY --from=builder /app/target/release/database /usr/local/bin/database

RUN useradd --system --uid 1000 database && mkdir -p /data && chown database:database /data
USER database

ENV DATABASE_DATA=/data
EXPOSE 5454
VOLUME ["/data"]

# `ping` connects to the server in this container with the admin password
# it was started with, and exits 0 once every database has been recovered.
HEALTHCHECK --interval=5s --timeout=3s --start-period=30s --retries=20 \
  CMD ["database", "ping"]

# Exec form, so the server is PID 1 and receives the SIGTERM that asks it
# to checkpoint and stop.
CMD ["database", "serve"]
