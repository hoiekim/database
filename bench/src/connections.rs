//! What a connection costs this server: the time to open one, to its
//! first answer, and the memory the server holds for each that is open.
//!
//! ```text
//! ./target/release/bench --connections
//! BENCH_TLS=1 ./target/release/bench --connections
//! BENCH_SOCKET=1 ./target/release/bench --connections
//! ```
//!
//! Under TLS the first is mostly the handshake, and the second the
//! session's keys and buffers.

use std::path::Path;
use std::time::{Duration, Instant};

use database_client::Client;

use crate::{memory, micros, start_ours};

/// How many connections are opened one after another to time one.
const TIMED: usize = 500;
/// How many are held open at once to weigh one.
const HELD: usize = 1_000;

async fn settled(pid: u32) -> u64 {
    // The server hands freed memory back some seconds after its last
    // request: read once that has happened.
    tokio::time::sleep(Duration::from_secs(7)).await;
    memory(pid)
}

pub async fn connections(binary: &Path) {
    let (server, client) = start_ours(binary, 32).await;
    let pid = server.pid().expect("the server is a process");
    let config = {
        let mut config = client.config().clone();
        config.pool_size = 1;
        config
    };
    let idle = settled(pid).await;

    let mut times = Vec::with_capacity(TIMED);
    for _ in 0..TIMED {
        let started = Instant::now();
        let one = Client::connect(config.clone()).await.expect("a connection");
        times.push(started.elapsed());
        drop(one);
    }
    times.sort();

    let before = settled(pid).await;
    let mut held = Vec::with_capacity(HELD);
    for _ in 0..HELD {
        held.push(Client::connect(config.clone()).await.expect("a connection"));
    }
    let with = settled(pid).await;
    // Each still answers: none was dropped to make room.
    for one in &held {
        one.ping().await.expect("a held connection answers");
    }

    let over = match (&config.tls, &config.socket) {
        (Some(_), _) => "TLS",
        (None, Some(_)) => "a Unix domain socket",
        (None, None) => "TCP in the clear",
    };
    println!("| Connections over {over} | |");
    println!("|---|---:|");
    println!("| Memory when idle, before any connection | {:.1} MB |", idle as f64 / 1e6);
    println!("| A new connection, to its first answer: median | {} |", micros(times[TIMED / 2]));
    println!("| A new connection, to its first answer: 99th percentile | {} |", micros(times[TIMED * 99 / 100]));
    println!("| Memory for each of {HELD} open connections | {:.1} kB |", with.saturating_sub(before) as f64 / HELD as f64 / 1e3);
}
