//! The `database` command: the server, and the tools that talk to it.
//!
//! ```text
//! database serve                       run the server
//! database ping                        exit 0 if the server answers
//! database create <name>               create a database; its password from DATABASE_PASSWORD
//! database drop <name>
//! database password <name>             change a database's password
//! database list
//! database dump [name...] > file       every database, or the named ones, to stdout
//! database restore [--replace] < file  load a dump from stdin
//! database check                       verify every database's files
//! database checkpoint
//! database stats
//! ```
//!
//! `serve` reads its settings from the environment:
//!
//! | Variable | Default | |
//! |---|---|---|
//! | `DATABASE_ADMIN_PASSWORD` | (required) | for every command but `serve` to reach the server |
//! | `DATABASE_DATA` | `./data` | where the files live |
//! | `DATABASE_LISTEN` | `0.0.0.0:5454` | address to listen on |
//! | `DATABASE_CACHE_MB` | `32` | memory for cached pages |
//!
//! The other commands connect to `DATABASE_HOST` (default `127.0.0.1`) on
//! the port of `DATABASE_LISTEN`, with `DATABASE_ADMIN_PASSWORD`: run
//! inside the server's container, they need nothing else.

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::process::ExitCode;

use database_client::{Client, Config, DbError, DEFAULT_PORT};
use database_server::{Server, ServerConfig};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn listen_address() -> Result<SocketAddr, String> {
    let text = env("DATABASE_LISTEN").unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}"));
    text.parse().map_err(|_| format!("DATABASE_LISTEN is not an address: {text}"))
}

fn admin_password() -> Result<String, String> {
    env("DATABASE_ADMIN_PASSWORD").ok_or_else(|| "DATABASE_ADMIN_PASSWORD is not set".to_string())
}

async fn admin() -> Result<Client, String> {
    let host = env("DATABASE_HOST").unwrap_or_else(|| "127.0.0.1".to_string());
    let mut config = Config::new(&host, listen_address()?.port(), "", &admin_password()?);
    config.name = "database-cli".to_string();
    config.pool_size = 1;
    // A dump or a restore of everything takes as long as it takes.
    config.statement_timeout = std::time::Duration::ZERO;
    Client::connect(config).await.map_err(|e| e.to_string())
}

fn text(e: DbError) -> String {
    e.to_string()
}

async fn serve() -> Result<(), String> {
    let mut config = ServerConfig::new(env("DATABASE_DATA").unwrap_or_else(|| "./data".to_string()), &admin_password()?);
    config.listen = listen_address()?;
    if let Some(mb) = env("DATABASE_CACHE_MB") {
        config.cache_bytes = mb.parse::<usize>().map_err(|_| "DATABASE_CACHE_MB is not a number".to_string())? * 1024 * 1024;
    }
    let data_dir = config.data_dir.clone();
    let server = Server::start(config).await.map_err(|e| format!("cannot start: {e}"))?;
    eprintln!("database: serving {} on {}", data_dir.display(), server.addr());
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|e| e.to_string())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    eprintln!("database: stopping");
    server.stop().await;
    eprintln!("database: stopped");
    Ok(())
}

/// The value after `flag` in the arguments, removing both.
fn take_option(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let at = args.iter().position(|a| a == flag)?;
    if at + 1 >= args.len() {
        return None;
    }
    args.remove(at);
    Some(args.remove(at))
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    match args.iter().position(|a| a == flag) {
        Some(at) => {
            args.remove(at);
            true
        }
        None => false,
    }
}

fn one_name(args: &[String], usage: &str) -> Result<String, String> {
    match args {
        [name] => Ok(name.clone()),
        _ => Err(format!("usage: database {usage}")),
    }
}

/// A database's password comes from the environment only: an argument
/// would show in the process list for as long as the command runs.
fn new_password(args: &mut Vec<String>) -> Result<String, String> {
    if take_option(args, "--password").is_some() || take_flag(args, "--password") {
        return Err("a password on the command line shows in the process list: give it in DATABASE_PASSWORD".to_string());
    }
    env("DATABASE_PASSWORD").ok_or_else(|| "give the database's password in DATABASE_PASSWORD".to_string())
}

async fn run(command: &str, mut args: Vec<String>) -> Result<(), String> {
    match command {
        "serve" => serve().await,
        "ping" => admin().await?.ping().await.map_err(text),
        "create" => {
            let if_not_exists = take_flag(&mut args, "--if-not-exists");
            let password = new_password(&mut args)?;
            let name = one_name(&args, "create [--if-not-exists] <name>")?;
            let client = admin().await?;
            if if_not_exists {
                client.admin().create_database_if_not_exists(&name, &password).await.map_err(text)
            } else {
                client.admin().create_database(&name, &password).await.map_err(text)
            }
        }
        "drop" => admin().await?.admin().drop_database(&one_name(&args, "drop <name>")?).await.map_err(text),
        "password" => {
            let password = new_password(&mut args)?;
            admin().await?.admin().set_password(&one_name(&args, "password <name>")?, &password).await.map_err(text)
        }
        "list" => {
            for name in admin().await?.admin().list_databases().await.map_err(text)? {
                println!("{name}");
            }
            Ok(())
        }
        "dump" => {
            if std::io::stdout().is_terminal() {
                return Err("stdout is a terminal; redirect the dump to a file or a pipe".to_string());
            }
            let names: Vec<&str> = args.iter().map(String::as_str).collect();
            let mut out = tokio::io::stdout();
            let bytes = admin().await?.admin().dump(&names, &mut out).await.map_err(text)?;
            eprintln!("database: dumped {bytes} bytes");
            Ok(())
        }
        "restore" => {
            let replace = take_flag(&mut args, "--replace");
            if !args.is_empty() {
                return Err("usage: database restore [--replace] < file".to_string());
            }
            if std::io::stdin().is_terminal() {
                return Err("stdin is a terminal; give the dump on it".to_string());
            }
            admin().await?.admin().restore(&mut tokio::io::stdin(), replace).await.map_err(text)?;
            eprintln!("database: restored");
            Ok(())
        }
        "check" => {
            print!("{}", admin().await?.admin().check().await.map_err(text)?);
            Ok(())
        }
        "checkpoint" => admin().await?.admin().checkpoint().await.map_err(text),
        "stats" => {
            print!("{}", admin().await?.admin().stats().await.map_err(text)?);
            Ok(())
        }
        _ => Err("usage: database serve | ping | create <name> | drop <name> | password <name> | list | dump [name...] | restore [--replace] | check | checkpoint | stats".to_string()),
    }
}

fn main() -> ExitCode {
    database_server::memory::configure();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let command = if args.is_empty() { String::new() } else { args.remove(0) };
    // Few threads, each with room for deeply nested expressions; the
    // blocking pool, where requests run, grows and shrinks with the load.
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(workers).thread_stack_size(8 * 1024 * 1024).max_blocking_threads(256).enable_all().build();
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("database: cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(&command, args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("database: {message}");
            ExitCode::FAILURE
        }
    }
}
