//! A server that follows another: the standby's side of it.
//!
//! Started with a primary's address, a server keeps a copy of each of
//! the primary's databases and answers reads from it. One task asks the
//! primary now and then which databases it has; for each, another keeps
//! the copy going:
//!
//! - **It begins the copy** where this server has none: from the
//!   archive, if this server reads the primary's and the history is
//!   there, and otherwise from a base the primary sends.
//! - **It asks for the log** from where the copy ends, and gives the
//!   engine each piece as it comes (`Database::receive`), which writes
//!   it, syncs it and applies it.
//! - **It asks again** whenever the stream ends: the primary restarted,
//!   the network was cut, this server restarted. The copy says where it
//!   ends, and the primary sends from there.
//! - **Told that it is behind** what the primary still keeps, it reads
//!   what the archive has, and if that does not reach, drops the copy
//!   and begins it again from a base.
//!
//! What it never does is touch a database it did not copy: one of this
//! server's own under a name the primary also has, a copy of another
//! history, a database that was promoted. Those it leaves, and says so
//! in `stats`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use database_client::{Client, Config, DbError, DbResult, ErrorCode, Response, Source};
use tokio::sync::{mpsc, watch};

use crate::State;

/// How often the primary is asked which databases it has.
const LOOK: Duration = Duration::from_secs(2);
/// How long a stream may say nothing before it is taken for dead. The
/// primary says where its log stands every second.
const SILENCE: Duration = Duration::from_secs(10);
/// How long to wait before asking again after being refused or cut off.
const AGAIN: Duration = Duration::from_millis(500);

/// Where a standby's primary is, and what it is told there.
#[derive(Clone, Debug)]
pub struct Primary {
    pub host: String,
    pub port: u16,
    /// The primary's administrator's password.
    pub password: String,
}

impl Primary {
    async fn connect(&self) -> DbResult<Client> {
        let mut config = Config::new(&self.host, self.port, "", &self.password);
        config.name = "standby".to_string();
        config.pool_size = 2;
        config.connect_timeout = Duration::from_secs(5);
        // A base, and the log, take as long as they take.
        config.statement_timeout = Duration::ZERO;
        Client::connect(config).await
    }
}

fn say(state: &State, database: &str, what: impl Into<String>) {
    let what = what.into();
    let mut said = state.standby.lock().unwrap();
    if said.get(database) != Some(&what) {
        eprintln!("database: standby: {database:?}: {what}");
        said.insert(database.to_string(), what);
    }
}

/// Keeps a copy of each of the primary's databases, until the server
/// stops.
pub(crate) async fn run(state: Arc<State>, primary: Primary, mut stopping: watch::Receiver<bool>) {
    let mut tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    loop {
        match look(&state, &primary).await {
            Ok(sources) => {
                say(&state, "", format!("following {}:{}", primary.host, primary.port));
                for source in sources {
                    if tasks.get(&source.name).is_some_and(|task| !task.is_finished()) {
                        continue;
                    }
                    let task = tokio::spawn(keep(state.clone(), primary.clone(), source.clone(), stopping.clone()));
                    tasks.insert(source.name, task);
                }
            }
            Err(e) => say(&state, "", format!("the primary at {}:{} does not answer: {}", primary.host, primary.port, e.message)),
        }
        tokio::select! {
            _ = tokio::time::sleep(LOOK) => {}
            _ = stopping.changed() => break,
        }
    }
    for (_, task) in tasks {
        let _ = task.await;
    }
}

/// Asks the primary which databases it has, and gives the copies here
/// the passwords those have there.
async fn look(state: &Arc<State>, primary: &Primary) -> DbResult<Vec<Source>> {
    let sources = primary.connect().await?.admin().sources().await?;
    let mut access = state.access.lock().unwrap();
    for source in &sources {
        let follows = state.engine.database(&source.name).is_ok_and(|db| db.followed().is_some());
        if follows && access.note(&source.name) != source.note {
            access.set_note(&source.name, &source.note).map_err(|e| DbError::new(ErrorCode::Io, e.to_string()))?;
        }
    }
    Ok(sources)
}

/// What ends one attempt at following a database.
enum Ended {
    /// Nothing more to do for this database here: it is this server's
    /// own, or another history's.
    Left(String),
    /// To be tried again, for this reason.
    Again(String),
}

/// Keeps the copy of one database going until the server stops or the
/// database is no longer this task's to keep.
async fn keep(state: Arc<State>, primary: Primary, source: Source, mut stopping: watch::Receiver<bool>) {
    loop {
        let attempt = tokio::select! {
            ended = follow(&state, &primary, &source) => ended,
            _ = stopping.changed() => return,
        };
        match attempt {
            Ended::Left(why) => {
                say(&state, &source.name, why);
                return;
            }
            Ended::Again(why) => say(&state, &source.name, why),
        }
        tokio::select! {
            _ = tokio::time::sleep(AGAIN) => {}
            _ = stopping.changed() => return,
        }
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Ended> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Ended::Again(e.to_string()))
}

/// One attempt: begins the copy if there is none, then takes the log
/// for as long as it comes.
async fn follow(state: &Arc<State>, primary: &Primary, source: &Source) -> Ended {
    match follow_once(state, primary, source).await {
        Ok(ended) | Err(ended) => ended,
    }
}

async fn follow_once(state: &Arc<State>, primary: &Primary, source: &Source) -> Result<Ended, Ended> {
    let name = source.name.clone();
    let lost = |e: DbError| Ended::Again(format!("not connected to the primary: {}", e.message));
    let client = primary.connect().await.map_err(lost)?;
    let db = match state.engine.database(&name) {
        Ok(db) => db,
        Err(_) => {
            begin(state, &client, source).await?;
            state.engine.database(&name).map_err(|e| Ended::Again(e.to_string()))?
        }
    };
    if db.followed().is_none() {
        return Ok(Ended::Left("is a database of this server's own, which takes writes, and is left as it is: the primary's database of that name is not followed".into()));
    }
    if db.uuid() != source.uuid {
        return Ok(Ended::Left("is a copy of another history than the primary's database of that name has now, and is left as it is: drop it here to follow the new one".into()));
    }
    state.access.lock().unwrap().set_note(&name, &source.note).map_err(|e| Ended::Again(e.to_string()))?;
    let (generation, offset) = db.followed().map(|followed| followed.received).unwrap_or_default();
    let mut stream = client.admin().follow(&name, source.uuid, generation, offset).await.map_err(lost)?;
    say(state, &name, "following");
    loop {
        let piece = match tokio::time::timeout(SILENCE, stream.next()).await {
            Err(_) => return Ok(Ended::Again(format!("the primary said nothing for {} seconds", SILENCE.as_secs()))),
            Ok(Err(e)) if e.code == ErrorCode::Behind => return behind(state, &name, e).await,
            Ok(Err(e)) if e.code == ErrorCode::Connection => return Ok(Ended::Again(format!("not connected to the primary: {}", e.message))),
            Ok(Err(e)) => return Ok(Ended::Again(format!("the primary does not send its log: {}", e.message))),
            Ok(Ok(piece)) => piece,
        };
        let Some(Response::Log { generation, offset, bytes, end, time }) = piece else {
            return Ok(Ended::Again("the primary answered with something else than its log".into()));
        };
        let db = db.clone();
        let taken = blocking(move || {
            db.heard_from_source(end, time);
            // A piece without bytes only says where the primary is,
            // unless it is the first of the next file.
            match db.followed() {
                Some(followed) if bytes.is_empty() && generation == followed.received.0 => Ok(()),
                Some(_) => db.receive(generation, offset, &bytes),
                // Promoted meanwhile.
                None => Ok(()),
            }
        })
        .await?;
        if let Err(e) = taken {
            if state.engine.database(&name).is_ok_and(|now| now.followed().is_none()) {
                return Ok(Ended::Left("was promoted, and follows nothing".into()));
            }
            return Ok(Ended::Again(format!("the primary's log was not taken: {e}")));
        }
    }
}

/// Begins the copy of a database this server does not have: from the
/// archive if it is there, and otherwise from a base the primary sends.
async fn begin(state: &Arc<State>, client: &Client, source: &Source) -> Result<(), Ended> {
    let name = source.name.clone();
    say(state, &name, "beginning a copy");
    let (engine_state, uuid, of) = (state.clone(), source.uuid, name.clone());
    match blocking(move || engine_state.engine.seed_from_archive(&of, &uuid)).await? {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        // An archive that cannot give it is no reason not to ask the
        // primary.
        Err(e) => say(state, &name, format!("the archive did not give a copy to begin from: {e}")),
    }
    let mut stream = client.admin().base_of(&name).await.map_err(|e| Ended::Again(format!("the primary sends no base: {}", e.message)))?;
    let (chunks, mut incoming) = mpsc::channel::<Vec<u8>>(4);
    let (engine_state, uuid, of) = (state.clone(), source.uuid, name.clone());
    let loader = tokio::task::spawn_blocking(move || {
        let mut seed = engine_state.engine.seed(&of)?;
        let mut whole = false;
        while let Some(chunk) = incoming.blocking_recv() {
            // An empty chunk is the sender's word that the base ended.
            if chunk.is_empty() {
                whole = true;
                break;
            }
            seed.feed(&chunk)?;
        }
        if !whole {
            return Err(database_engine::error::Error::db(ErrorCode::Connection, "the base did not arrive whole"));
        }
        seed.finish(&uuid)
    });
    let sent = loop {
        match tokio::time::timeout(SILENCE.saturating_mul(6), stream.next()).await {
            Ok(Ok(Some(Response::Chunk(bytes)))) => {
                if chunks.send(bytes).await.is_err() {
                    break Ok(());
                }
            }
            Ok(Ok(None)) => break chunks.send(Vec::new()).await.map_err(|_| "the copy stopped early".to_string()),
            Ok(Ok(Some(_))) => break Err("the primary answered with something else than a base".to_string()),
            Ok(Err(e)) => break Err(e.message),
            Err(_) => break Err("the primary stopped sending the base".to_string()),
        }
    };
    drop(chunks);
    let loaded = loader.await.map_err(|e| Ended::Again(e.to_string()))?;
    sent.map_err(|why| Ended::Again(format!("no base to begin from: {why}")))?;
    loaded.map_err(|e| Ended::Again(format!("no base to begin from: {e}")))
}

/// The primary no longer keeps the log from where the copy ends. What
/// the archive holds of it is read; if that does not reach either, the
/// copy is dropped, to be begun again from a base.
async fn behind(state: &Arc<State>, name: &str, told: DbError) -> Result<Ended, Ended> {
    let (engine_state, of) = (state.clone(), name.to_string());
    let caught = blocking(move || engine_state.engine.catch_up_from_archive(&of)).await?;
    if matches!(caught, Ok(true)) {
        return Ok(Ended::Again("behind what the primary keeps: read on from the archive".into()));
    }
    let (engine_state, of) = (state.clone(), name.to_string());
    let dropped = blocking(move || {
        // Only a copy is dropped: never a database that takes writes.
        match engine_state.engine.database(&of) {
            Ok(db) if db.followed().is_some() => engine_state.engine.drop_database(&of),
            _ => Ok(()),
        }
    })
    .await?;
    match dropped {
        Ok(()) => Ok(Ended::Again(format!("behind what the primary keeps ({}), and the archive does not reach: the copy is begun again from a base", told.message))),
        Err(e) => Ok(Ended::Again(format!("behind what the primary keeps, and the copy could not be dropped to begin again: {e}"))),
    }
}
