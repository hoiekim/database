//! A large insert on its own: the statement of a bulk load.
//!
//! Most of what such an insert costs, where its table has a full-text
//! index, is cutting its texts into words, and that needs nothing but
//! the texts. Done under the database's one turn to write, as every
//! other part of the insert must be, it kept every other writer waiting
//! for a tenth of a second per megabyte. So it is done here first, on
//! the connection's own thread, beside whoever has the turn; several
//! connections that load at once then cut on as many cores, and take
//! turns only to store rows and the words already cut.
//!
//! The words are used for a row only if the row's text, when it is
//! written, is the text they were cut from (`fts::RowAhead`): what was
//! cut ahead can save work, never decide what is stored.

use database_engine::fts;

use super::*;

/// The statement, with its turn held: begun, run and committed as far as
/// the log. Returns as `lone_write` does.
fn write(db: &Arc<Database>, insert: &database_protocol::query::Insert, ahead: &fts::Ahead, deadline: Option<Instant>, wait: Duration) -> (DbResult<Vec<Response>>, u64, Read) {
    let mut txn = match begin_write(db, wait, false) {
        Ok(txn) => txn,
        Err(e) => return (Err(e), 0, Read::default()),
    };
    let base = txn.base();
    let outcome = txn.insert_ahead(insert, Some(ahead), deadline);
    let read = txn.take_read();
    match outcome {
        Ok(set) => match txn.commit_start() {
            Ok(committed) => (Ok(rows(set)), committed.map_or(base, |c| c.version()), read),
            Err(e) => (Err(e.into_db()), base, read),
        },
        Err(e) => {
            drop(txn);
            (Err(e.into_db()), base, read)
        }
    }
}

impl Session {
    /// Answers a large insert that is a transaction of its own, into a
    /// table with text to cut into words. Any other request is handed
    /// back, to be answered the usual way.
    pub(super) async fn load(&mut self, request: Request, deadline: Option<Instant>) -> Result<Vec<Response>, Request> {
        let Request::Insert(insert) = &request else { return Err(request) };
        if self.txn.is_some() || brief(&request, self.request_bytes) || !matches!(insert.source, InsertSource::Values(_)) {
            return Err(request);
        }
        let Ok(db) = self.database() else { return Err(request) };
        // Timed, and logged if slow, as any other statement is
        // (`timed`): from before its words are cut.
        let started = Instant::now();
        // The table as the newest readable version has it. Should it
        // change before the insert's turn comes, words were cut for
        // nothing, and the rows are cut as they are written.
        let catalog = db.begin_read().catalog().clone();
        let Some(ahead) = tokio::task::block_in_place(|| fts::cut_ahead(&catalog, insert)) else { return Err(request) };
        drop(catalog);
        // Described only where slow statements are logged at all.
        let described = self.state.config.slow_statement.map(|_| shape(&request));
        let Request::Insert(insert) = request else { unreachable!("seen above to be an insert") };
        self.read = Read::default();
        let answer = self.loaded(db, insert, ahead, deadline).await;
        self.slow(started, || described.unwrap_or_default());
        Ok(answer.unwrap_or_else(|e| vec![Response::Error(e)]))
    }

    async fn loaded(&mut self, db: Arc<Database>, insert: database_protocol::query::Insert, ahead: fts::Ahead, deadline: Option<Instant>) -> DbResult<Vec<Response>> {
        let wait = self.state.config.lock_wait;
        let turn = self.turn(wait).await?;
        let writing = db.clone();
        // The insert and its words come back with the outcome, to be
        // taken apart here and not while the turn is held.
        let ((result, version, read), _insert, _ahead) = blocking(move || {
            let outcome = write(&writing, &insert, &ahead, deadline, wait);
            (outcome, insert, ahead)
        })
        .await?;
        drop(turn);
        self.read = read;
        durable(&db, version).await?;
        result
    }
}
