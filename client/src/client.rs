//! The client: statements, transactions, named locks, administration.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use database_protocol::error::{DbError, DbResult, ErrorCode};
use database_protocol::expr::Expr;
use database_protocol::message::{Admin as AdminRequest, Ddl, Request, Response, TxnKind};
use database_protocol::query::{Delete, Insert, Query, Update};
use database_protocol::schema::{ColumnDef, IndexDef, TableDef};
use database_protocol::value::Timestamp;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::conn::{Pool, PooledConn};
use crate::row::{Row, Rows};

/// How many times `Client::transaction` runs its work beside other
/// writers before it runs it as the only one.
const OPTIMISTIC_RUNS: usize = 2;

fn unexpected(what: &str) -> DbError {
    DbError::new(ErrorCode::Connection, format!("the server answered {what} with something else"))
}

fn rows(response: Response) -> DbResult<Rows> {
    match response {
        Response::Rows { set, .. } => Ok(Rows::new(set)),
        _ => Err(unexpected("a statement")),
    }
}

fn ok(response: Response) -> DbResult<()> {
    match response {
        Response::Ok => Ok(()),
        _ => Err(unexpected("a command")),
    }
}

/// What a `Client` and a `Transaction` have in common, so code that reads
/// and writes can be handed either: `async fn find(db: &impl Db, ..)`.
///
/// On a `Client` each statement is its own transaction: it takes effect
/// whole or not at all, and is durable when the call returns.
pub trait Db: Sync {
    /// Sends one request and returns its answer. The other methods are
    /// built on this.
    fn call(&self, request: Request) -> impl Future<Output = DbResult<Response>> + Send;

    /// Runs a query.
    fn query(&self, query: Query) -> impl Future<Output = DbResult<Rows>> + Send {
        async move { rows(self.call(Request::Query(query)).await?) }
    }

    /// Runs a query that gives at most one row.
    fn query_opt(&self, query: Query) -> impl Future<Output = DbResult<Option<Row>>> + Send {
        async move { Ok(self.query(query).await?.opt()) }
    }

    /// Runs a query that must give exactly one row.
    fn query_one(&self, query: Query) -> impl Future<Output = DbResult<Row>> + Send {
        async move { self.query(query).await?.one() }
    }

    /// The row with this primary key.
    fn get<E: Into<Expr> + Send>(&self, table: &str, key: impl IntoIterator<Item = E> + Send) -> impl Future<Output = DbResult<Option<Row>>> + Send {
        let query = Query::table(table).key(key);
        async move { self.query_opt(query).await }
    }

    /// Inserts rows. The result holds what `returning` asked for and, as
    /// `affected`, how many rows were inserted or updated.
    fn insert(&self, insert: Insert) -> impl Future<Output = DbResult<Rows>> + Send {
        async move { rows(self.call(Request::Insert(insert)).await?) }
    }

    fn update(&self, update: Update) -> impl Future<Output = DbResult<Rows>> + Send {
        async move { rows(self.call(Request::Update(update)).await?) }
    }

    fn delete(&self, delete: Delete) -> impl Future<Output = DbResult<Rows>> + Send {
        async move { rows(self.call(Request::Delete(delete)).await?) }
    }

    /// Creates a table. An error if one of that name exists.
    fn create_table(&self, def: TableDef) -> impl Future<Output = DbResult<()>> + Send {
        async move { ok(self.call(Request::Ddl(Ddl::CreateTable { def, if_not_exists: false })).await?) }
    }

    /// Creates a table unless one of that name exists. Does not compare
    /// the definitions: use `describe` and `add_column` to bring an
    /// existing table up to date.
    fn create_table_if_not_exists(&self, def: TableDef) -> impl Future<Output = DbResult<()>> + Send {
        async move { ok(self.call(Request::Ddl(Ddl::CreateTable { def, if_not_exists: true })).await?) }
    }

    fn drop_table(&self, name: &str) -> impl Future<Output = DbResult<()>> + Send {
        let name = name.to_string();
        async move { ok(self.call(Request::Ddl(Ddl::DropTable { name, if_exists: false })).await?) }
    }

    fn drop_table_if_exists(&self, name: &str) -> impl Future<Output = DbResult<()>> + Send {
        let name = name.to_string();
        async move { ok(self.call(Request::Ddl(Ddl::DropTable { name, if_exists: true })).await?) }
    }

    /// Adds a column. Existing rows read its default, or NULL.
    fn add_column(&self, table: &str, column: ColumnDef) -> impl Future<Output = DbResult<()>> + Send {
        let table = table.to_string();
        async move { ok(self.call(Request::Ddl(Ddl::AddColumn { table, column, if_not_exists: false })).await?) }
    }

    fn add_column_if_not_exists(&self, table: &str, column: ColumnDef) -> impl Future<Output = DbResult<()>> + Send {
        let table = table.to_string();
        async move { ok(self.call(Request::Ddl(Ddl::AddColumn { table, column, if_not_exists: true })).await?) }
    }

    fn drop_column(&self, table: &str, column: &str) -> impl Future<Output = DbResult<()>> + Send {
        let (table, column) = (table.to_string(), column.to_string());
        async move { ok(self.call(Request::Ddl(Ddl::DropColumn { table, column, if_exists: false })).await?) }
    }

    /// Adds an index and fills it from the rows already there. A unique
    /// index fails with `UniqueViolation` if they hold duplicates.
    fn create_index(&self, table: &str, index: IndexDef) -> impl Future<Output = DbResult<()>> + Send {
        let table = table.to_string();
        async move { ok(self.call(Request::Ddl(Ddl::CreateIndex { table, index, if_not_exists: false })).await?) }
    }

    fn create_index_if_not_exists(&self, table: &str, index: IndexDef) -> impl Future<Output = DbResult<()>> + Send {
        let table = table.to_string();
        async move { ok(self.call(Request::Ddl(Ddl::CreateIndex { table, index, if_not_exists: true })).await?) }
    }

    fn drop_index(&self, table: &str, index: &str) -> impl Future<Output = DbResult<()>> + Send {
        let (table, index) = (table.to_string(), index.to_string());
        async move { ok(self.call(Request::Ddl(Ddl::DropIndex { table, index, if_exists: false })).await?) }
    }

    fn drop_index_if_exists(&self, table: &str, index: &str) -> impl Future<Output = DbResult<()>> + Send {
        let (table, index) = (table.to_string(), index.to_string());
        async move { ok(self.call(Request::Ddl(Ddl::DropIndex { table, index, if_exists: true })).await?) }
    }

    /// Every table's definition as the server holds it.
    fn describe(&self) -> impl Future<Output = DbResult<Vec<TableDef>>> + Send {
        async move {
            match self.call(Request::Describe).await? {
                Response::Tables(tables) => Ok(tables),
                _ => Err(unexpected("describe")),
            }
        }
    }

    /// One table's definition, if the table exists.
    fn describe_table(&self, name: &str) -> impl Future<Output = DbResult<Option<TableDef>>> + Send {
        let name = name.to_string();
        async move { Ok(self.describe().await?.into_iter().find(|t| t.name == name)) }
    }

    /// The server's clock. Inside a transaction, the moment it began.
    fn now(&self) -> impl Future<Output = DbResult<Timestamp>> + Send {
        async move {
            match self.call(Request::Now).await? {
                Response::Time(t) => Ok(Timestamp(t)),
                _ => Err(unexpected("now")),
            }
        }
    }
}

/// A handle on the database, cheap to clone and share between tasks. It
/// keeps a small pool of connections and opens them as they are needed.
#[derive(Clone)]
pub struct Client {
    pool: Arc<Pool>,
}

impl Client {
    /// Connects, checking the address and the password with one
    /// connection.
    pub async fn connect(config: Config) -> DbResult<Client> {
        let client = Client { pool: Arc::new(Pool::new(config)) };
        client.ping().await?;
        Ok(client)
    }

    pub async fn connect_url(url: &str) -> DbResult<Client> {
        Client::connect(Config::from_url(url)?).await
    }

    pub fn config(&self) -> &Config {
        &self.pool.config
    }

    /// Whether the server answers.
    pub async fn ping(&self) -> DbResult<()> {
        ok(self.call(Request::Ping).await?)
    }

    /// Closes idle connections. The client can still be used and will
    /// reconnect.
    pub fn close(&self) {
        self.pool.clear();
    }

    /// Starts a transaction that is the database's only writer until it
    /// ends. It waits for any other writer to finish, and every other
    /// writer then waits for it: keep it short, and do not wait on
    /// anything but the database inside it. Its commit cannot conflict.
    pub async fn begin(&self) -> DbResult<Transaction> {
        self.begin_with(TxnKind::Exclusive).await
    }

    /// Starts a transaction that may write and holds nobody up: it runs
    /// beside other writers, on a copy of the database that only it
    /// sees. Its `commit` checks that no commit made meanwhile has
    /// changed anything it was told; if one has, the commit fails with
    /// `ErrorCode::Conflict`, nothing of the transaction takes effect,
    /// and it can be run again. `transaction` does all of that.
    pub async fn begin_optimistic(&self) -> DbResult<Transaction> {
        self.begin_with(TxnKind::Optimistic).await
    }

    /// Starts a transaction that only reads. It sees the database as of
    /// this moment whatever is written meanwhile, and blocks nobody.
    pub async fn begin_read(&self) -> DbResult<Transaction> {
        self.begin_with(TxnKind::Read).await
    }

    async fn begin_with(&self, kind: TxnKind) -> DbResult<Transaction> {
        let mut conn = self.pool.acquire().await?;
        let lock_wait_ms = self.pool.config.lock_wait.as_millis() as u64;
        ok(conn
            .conn()
            .call(Request::Begin { kind, lock_wait_ms })
            .await?)?;
        conn.held = true;
        Ok(Transaction {
            inner: Arc::new(TxInner {
                conn: Mutex::new(Some(conn)),
                conflicted: AtomicBool::new(false),
            }),
        })
    }

    /// Runs `work` in a writing transaction: commits if it returns `Ok`,
    /// rolls back if it returns `Err` or panics. The result is as if
    /// nothing else had written while it ran.
    ///
    /// `work` may be called more than once. It runs first beside other
    /// writers (see `begin_optimistic`), so that it holds nobody up
    /// while it waits for its statements' answers. If another writer's
    /// commit then proves to have changed something it read, that run is
    /// undone whole and `work` is called again: once more the same way,
    /// and after that as the database's only writer, which cannot fail
    /// so. `work` should therefore do nothing but talk to the database
    /// through the transaction it is given; use `exclusive` for one that
    /// must run exactly once.
    ///
    /// ```ignore
    /// let id = db.transaction(|tx| async move {
    ///     let row = tx.insert(Insert::into("users").set("name", "ann").returning(["id"])).await?.one()?;
    ///     tx.insert(Insert::into("audit").set("action", "user.create")).await?;
    ///     row.get::<i64>("id")
    /// }).await?;
    /// ```
    pub async fn transaction<T, E, F, Fut>(&self, mut work: F) -> Result<T, E>
    where
        F: FnMut(Transaction) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: From<DbError>,
    {
        for _ in 0..OPTIMISTIC_RUNS {
            let tx = self.begin_optimistic().await?;
            match work(tx.clone()).await {
                Ok(value) => match tx.commit().await {
                    Ok(()) => return Ok(value),
                    Err(e) if e.code == ErrorCode::Conflict => {}
                    Err(e) => return Err(e.into()),
                },
                Err(e) => {
                    let conflicted = tx.conflicted();
                    let _ = tx.rollback().await;
                    if !conflicted {
                        return Err(e);
                    }
                }
            }
        }
        self.exclusive(work).await
    }

    /// Runs `work`, once, in a transaction that is the database's only
    /// writer while it runs (see `begin`): commits if it returns `Ok`,
    /// rolls back if it returns `Err` or panics.
    pub async fn exclusive<T, E, F, Fut>(&self, work: F) -> Result<T, E>
    where
        F: FnOnce(Transaction) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: From<DbError>,
    {
        let tx = self.begin().await?;
        match work(tx.clone()).await {
            Ok(value) => {
                tx.commit().await?;
                Ok(value)
            }
            Err(e) => {
                let _ = tx.rollback().await;
                Err(e)
            }
        }
    }

    /// The same for a transaction that only reads: `work` sees one moment
    /// of the database.
    pub async fn read<T, E, F, Fut>(&self, work: F) -> Result<T, E>
    where
        F: FnOnce(Transaction) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: From<DbError>,
    {
        let tx = self.begin_read().await?;
        let result = work(tx.clone()).await;
        let _ = tx.rollback().await;
        result
    }

    /// Takes the named lock, waiting as long as it takes. The lock belongs
    /// to this guard, not to a transaction: it is held until `unlock`, or
    /// until the guard is dropped or the process dies. For work that only
    /// one process should do at a time, such as migrations.
    pub async fn lock(&self, name: &str) -> DbResult<LockGuard> {
        loop {
            // Wait in slices, so a statement timeout cannot cut the wait.
            if let Some(guard) = self.lock_for(name, 5_000).await? {
                return Ok(guard);
            }
        }
    }

    /// Takes the named lock if nobody holds it.
    pub async fn try_lock(&self, name: &str) -> DbResult<Option<LockGuard>> {
        self.lock_for(name, 0).await
    }

    async fn lock_for(&self, name: &str, wait_ms: u64) -> DbResult<Option<LockGuard>> {
        let mut conn = self.pool.acquire().await?;
        match conn.conn().call(Request::Lock { name: name.to_string(), wait_ms }).await? {
            Response::Flag(true) => {
                conn.held = true;
                Ok(Some(LockGuard { conn: Some(conn), name: name.to_string() }))
            }
            Response::Flag(false) => Ok(None),
            _ => Err(unexpected("lock")),
        }
    }

    /// Server-wide operations. They need the administrator's password.
    pub fn admin(&self) -> Admin<'_> {
        Admin { client: self }
    }
}

impl Db for Client {
    async fn call(&self, request: Request) -> DbResult<Response> {
        let mut conn = self.pool.acquire().await?;
        conn.conn().call(request).await
    }
}

struct TxInner {
    conn: Mutex<Option<PooledConn>>,
    /// Whether the server has said that the transaction conflicted.
    conflicted: AtomicBool,
}

/// An open transaction. Statements run through it see each other's
/// effects; nobody else sees them until `commit`. Dropped without a
/// commit, it is rolled back.
///
/// After a statement fails, the transaction can only be rolled back, or
/// rolled back to a savepoint taken before the failure.
#[derive(Clone)]
pub struct Transaction {
    inner: Arc<TxInner>,
}

fn ended() -> DbError {
    DbError::new(ErrorCode::TransactionAborted, "the transaction has already ended")
}

impl Transaction {
    /// Makes the transaction's changes permanent and visible. When this
    /// returns `Ok` they are on disk.
    pub async fn commit(self) -> DbResult<()> {
        self.finish(Request::Commit).await
    }

    pub async fn rollback(self) -> DbResult<()> {
        self.finish(Request::Rollback).await
    }

    async fn finish(self, request: Request) -> DbResult<()> {
        let mut slot = self.inner.conn.lock().await;
        let mut conn = slot.take().ok_or_else(ended)?;
        // Whatever the answer, the server has ended the transaction: an
        // error on commit means it was rolled back.
        let result = conn.conn().call(request).await;
        conn.held = false;
        ok(self.inner.note(result)?)
    }

    /// Whether the server has ended the transaction because a commit
    /// made beside it changed something it had read: it can be run
    /// again. Only an optimistic transaction ever is.
    pub fn conflicted(&self) -> bool {
        self.inner.conflicted.load(Ordering::Relaxed)
    }

    /// Marks a point to return to with `rollback_to`.
    pub async fn savepoint(&self, name: &str) -> DbResult<()> {
        ok(self.call(Request::Savepoint(name.to_string())).await?)
    }

    /// Undoes everything since the savepoint, which stays in place. Also
    /// clears the failed state a statement error left behind.
    pub async fn rollback_to(&self, name: &str) -> DbResult<()> {
        ok(self.call(Request::RollbackTo(name.to_string())).await?)
    }

    /// Forgets the savepoint, keeping what was done since.
    pub async fn release(&self, name: &str) -> DbResult<()> {
        ok(self.call(Request::Release(name.to_string())).await?)
    }
}

impl TxInner {
    fn note(&self, result: DbResult<Response>) -> DbResult<Response> {
        if matches!(&result, Err(e) if e.code == ErrorCode::Conflict) {
            self.conflicted.store(true, Ordering::Relaxed);
        }
        result
    }
}

impl Db for Transaction {
    async fn call(&self, request: Request) -> DbResult<Response> {
        let mut slot = self.inner.conn.lock().await;
        let conn = slot.as_mut().ok_or_else(ended)?;
        let result = conn.conn().call(request).await;
        self.inner.note(result)
    }
}

/// A named lock held on the server. See `Client::lock`.
pub struct LockGuard {
    conn: Option<PooledConn>,
    name: String,
}

impl LockGuard {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Gives the lock up. Dropping the guard does the same by closing its
    /// connection.
    pub async fn unlock(mut self) -> DbResult<()> {
        let mut conn = self.conn.take().expect("held until unlocked");
        let result = conn.conn().call(Request::Unlock { name: self.name.clone() }).await;
        if result.is_ok() {
            conn.held = false;
        }
        ok(result?)
    }
}

/// Server-wide operations. See `Client::admin`.
pub struct Admin<'a> {
    client: &'a Client,
}

impl Admin<'_> {
    async fn call(&self, request: AdminRequest) -> DbResult<Response> {
        self.client.call(Request::Admin(request)).await
    }

    pub async fn create_database(&self, name: &str, password: &str) -> DbResult<()> {
        ok(self.call(AdminRequest::CreateDatabase { name: name.into(), password: password.into(), if_not_exists: false }).await?)
    }

    pub async fn create_database_if_not_exists(&self, name: &str, password: &str) -> DbResult<()> {
        ok(self.call(AdminRequest::CreateDatabase { name: name.into(), password: password.into(), if_not_exists: true }).await?)
    }

    /// Removes a database and its files. Connections to it are refused
    /// from then on.
    pub async fn drop_database(&self, name: &str) -> DbResult<()> {
        ok(self.call(AdminRequest::DropDatabase { name: name.into(), if_exists: false }).await?)
    }

    pub async fn drop_database_if_exists(&self, name: &str) -> DbResult<()> {
        ok(self.call(AdminRequest::DropDatabase { name: name.into(), if_exists: true }).await?)
    }

    pub async fn set_password(&self, name: &str, password: &str) -> DbResult<()> {
        ok(self.call(AdminRequest::SetPassword { name: name.into(), password: password.into() }).await?)
    }

    pub async fn list_databases(&self) -> DbResult<Vec<String>> {
        match self.call(AdminRequest::ListDatabases).await? {
            Response::Names(names) => Ok(names),
            _ => Err(unexpected("list databases")),
        }
    }

    /// Moves everything the log holds into the data file. The server does
    /// this by itself; tests and backups of the raw files call it.
    pub async fn checkpoint(&self) -> DbResult<()> {
        ok(self.call(AdminRequest::Checkpoint).await?)
    }

    /// Verifies the files of the connection's database, or of every
    /// database on an administering connection. Returns a report; an
    /// error if anything is wrong.
    pub async fn check(&self) -> DbResult<String> {
        match self.call(AdminRequest::Check).await? {
            Response::Text(report) => Ok(report),
            _ => Err(unexpected("check")),
        }
    }

    pub async fn stats(&self) -> DbResult<String> {
        match self.call(AdminRequest::Stats).await? {
            Response::Text(report) => Ok(report),
            _ => Err(unexpected("stats")),
        }
    }

    /// Writes a dump of the named databases, or of all of them when none
    /// is named, to `out`. The dump is each database as of one moment,
    /// taken without stopping writers. Returns the bytes written.
    pub async fn dump<W: AsyncWrite + Unpin>(&self, databases: &[&str], out: &mut W) -> DbResult<u64> {
        let mut conn = self.client.pool.acquire().await?;
        let request = Request::Admin(AdminRequest::Dump { databases: databases.iter().map(|d| d.to_string()).collect() });
        conn.conn().send(&request).await?;
        let mut written = 0u64;
        loop {
            match conn.conn().receive().await? {
                Response::Chunk(bytes) => {
                    out.write_all(&bytes).await.map_err(|e| DbError::new(ErrorCode::Io, format!("writing the dump: {e}")))?;
                    written += bytes.len() as u64;
                }
                Response::Ok => {
                    conn.conn().settle();
                    out.flush().await.map_err(|e| DbError::new(ErrorCode::Io, format!("writing the dump: {e}")))?;
                    return Ok(written);
                }
                Response::Error(e) => {
                    conn.conn().settle();
                    return Err(e);
                }
                _ => return Err(unexpected("dump")),
            }
        }
    }

    /// Loads a dump read from `input`. Each database in it is created;
    /// one that already exists is an error unless `replace`, which swaps
    /// it for the dump's. Nothing is changed unless the whole dump is
    /// read and verified.
    pub async fn restore<R: AsyncRead + Unpin>(&self, input: &mut R, replace: bool) -> DbResult<()> {
        let mut conn = self.client.pool.acquire().await?;
        // Until the restore ends, the connection holds a half-loaded dump.
        conn.held = true;
        ok(conn.conn().call(Request::Admin(AdminRequest::RestoreBegin { replace })).await?)?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = input.read(&mut buf).await.map_err(|e| DbError::new(ErrorCode::Io, format!("reading the dump: {e}")))?;
            if n == 0 {
                break;
            }
            ok(conn.conn().call(Request::Admin(AdminRequest::RestoreChunk(buf[..n].to_vec()))).await?)?;
        }
        let result = conn.conn().call(Request::Admin(AdminRequest::RestoreEnd)).await;
        conn.held = false;
        ok(result?)
    }
}
