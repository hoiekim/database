//! The client: statements, transactions, named locks, administration.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use database_protocol::error::{DbError, DbResult, ErrorCode};
use database_protocol::expr::Expr;
use database_protocol::message::{Admin as AdminRequest, BatchItem, Ddl, Request, Response, TxnKind, EXPLAIN_COLUMNS};
use database_protocol::query::{Delete, Insert, Query, Update};
use database_protocol::rights::Rights;
use database_protocol::schema::{ColumnDef, IndexDef, TableDef};
use database_protocol::value::{Timestamp, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::conn::{Pool, PooledConn};
use crate::row::{Row, Rows};

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

/// What a query made the server read: the answer of `Db::explain`.
///
/// There is no planner here, so there is no plan to look at: a query
/// reads the range it names, in order, and filters what that gives. What
/// can be wrong with it is that the range takes in far more than the
/// query returns, and these figures say so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Explained {
    /// Entries read from indexes: those of an ordered index's range, the
    /// children a tree walk found, a search's hits.
    pub index_entries: u64,
    /// Rows read from tables: in key order, through an index, and by
    /// joins and subqueries.
    pub rows_read: u64,
    /// Rows the query returned.
    pub rows_returned: u64,
    /// How long the server took over it.
    pub took: std::time::Duration,
}

fn explained(response: Response) -> DbResult<Explained> {
    let rows = rows(response)?;
    let row = rows.one()?;
    let figure = |n: usize| row.get::<i64>(EXPLAIN_COLUMNS[n]).map(|v| v.max(0) as u64);
    Ok(Explained { index_entries: figure(0)?, rows_read: figure(1)?, rows_returned: figure(2)?, took: std::time::Duration::from_micros(figure(3)?) })
}

/// A query, an insert, an update or a delete: what can be prepared, and
/// what a batch holds.
pub trait Statement {
    #[doc(hidden)]
    fn into_request(self) -> Request;
}

impl Statement for Query {
    fn into_request(self) -> Request {
        Request::Query(self)
    }
}

impl Statement for Insert {
    fn into_request(self) -> Request {
        Request::Insert(self)
    }
}

impl Statement for Update {
    fn into_request(self) -> Request {
        Request::Update(self)
    }
}

impl Statement for Delete {
    fn into_request(self) -> Request {
        Request::Delete(self)
    }
}

/// A statement to run many times with different values, as a prepared
/// statement is in SQL. Where the values go it says `param(0)`,
/// `param(1)`, and so on; `Db::execute` then runs it with a value for
/// each.
///
/// Each connection sends the statement to the server once, which keeps
/// it as it is decoded; after that only its number and the values
/// travel. That is most of what a small request costs either side, so it
/// is worth doing for whatever an application runs often: build each such
/// statement once, when the application starts, and keep it.
///
/// ```ignore
/// let by_token = Prepared::new(
///     Query::table("sessions").index("sessions_token", KeyRange::eq([param(0)])).filter(col("expires_at").gt(now())),
/// );
/// let session = db.execute(&by_token, [token_hash]).await?.opt();
/// ```
///
/// Cheap to clone and to share between tasks. It belongs to no client or
/// connection, and may be used with any, inside a transaction or not.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// What connections know it by: unlike any other's in this process.
    id: u64,
    statement: Arc<Request>,
}

impl Prepared {
    pub fn new(statement: impl Statement) -> Prepared {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Prepared { id: NEXT.fetch_add(1, Ordering::Relaxed), statement: Arc::new(statement.into_request()) }
    }

    /// The request that runs it with these values. The statement goes
    /// along; a connection that has sent it before leaves it out.
    fn run<V: Into<Value>>(&self, params: impl IntoIterator<Item = V>) -> Request {
        Request::Prepared { id: self.id, statement: Some(self.statement.clone()), params: params.into_iter().map(Into::into).collect() }
    }
}

/// Statements to run as one transaction, sent to the server whole: see
/// `Client::batch`.
#[derive(Clone, Debug, Default)]
pub struct Batch {
    items: Vec<BatchItem>,
}

impl Batch {
    pub fn new() -> Batch {
        Batch::default()
    }

    /// Adds a statement, to run after those added before it.
    #[allow(clippy::should_implement_trait)]
    pub fn add(mut self, statement: impl Statement) -> Batch {
        self.items.push(BatchItem { statement: statement.into_request(), expect: None });
        self
    }

    /// Adds a prepared statement with the values to run it with.
    pub fn add_prepared<V: Into<Value>>(mut self, prepared: &Prepared, params: impl IntoIterator<Item = V>) -> Batch {
        self.items.push(BatchItem { statement: prepared.run(params), expect: None });
        self
    }

    /// Says how many rows the statement added last must affect (a query:
    /// return). If it affects any other number the batch stops there
    /// with `ErrorCode::Unmet`, and nothing of it takes effect. This is
    /// how a later statement is made to depend on an earlier one: debit
    /// an account `filter`ed on its balance and `expect(1)`, and the
    /// credit after it happens only if the debit did.
    pub fn expect(mut self, rows: u64) -> Batch {
        if let Some(last) = self.items.last_mut() {
            last.expect = Some(rows);
        }
        self
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
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

    /// Runs a query and says what it made the server read, instead of
    /// giving its rows: see `Explained`. For finding out why a query is
    /// slow, or whether one will be when the table has grown.
    fn explain(&self, query: Query) -> impl Future<Output = DbResult<Explained>> + Send {
        async move { explained(self.call(Request::Explain(Box::new(Request::Query(query)))).await?) }
    }

    /// The same for a prepared query run with these values.
    fn explain_prepared<V: Into<Value>>(&self, prepared: &Prepared, params: impl IntoIterator<Item = V>) -> impl Future<Output = DbResult<Explained>> + Send {
        let request = Request::Explain(Box::new(prepared.run(params)));
        async move { explained(self.call(request).await?) }
    }

    /// Runs a prepared statement with a value for each of its `param`s,
    /// in order: see `Prepared`. The result is what the statement gives
    /// when run as it is: a query's rows, or what a write returns.
    fn execute<V: Into<Value>>(&self, prepared: &Prepared, params: impl IntoIterator<Item = V>) -> impl Future<Output = DbResult<Rows>> + Send {
        let request = prepared.run(params);
        async move { rows(self.call(request).await?) }
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
        ok(conn.conn().call(Request::Begin { kind, lock_wait_ms }).await?)?;
        conn.held = true;
        Ok(Transaction::on(conn, None))
    }

    /// A transaction that begins on the server with its first statement:
    /// the two travel as one request. Until then nothing has been sent,
    /// and it reads and holds nothing.
    async fn begin_later(&self, kind: TxnKind) -> DbResult<Transaction> {
        let conn = self.pool.acquire().await?;
        let lock_wait_ms = self.pool.config.lock_wait.as_millis() as u64;
        Ok(Transaction::on(conn, Some((kind, lock_wait_ms))))
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
    /// How many runs it gets beside the others is `Config::optimistic_runs`,
    /// 2 unless set. While more than a third of this client's
    /// transactions conflict on their first run, a second such run would
    /// mostly conflict again, and a transaction that conflicts goes
    /// straight to the write lock instead.
    ///
    /// The transaction begins on the server with its first statement, not
    /// when `work` is called: the two are sent as one request. What it
    /// sees of the database is as of that statement. (For statements
    /// that need no answers from each other, `batch` sends the whole
    /// transaction as one.)
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
        for run in 0..self.pool.config.optimistic_runs {
            if run > 0 && self.pool.contended() {
                break;
            }
            let tx = self.begin_later(TxnKind::Optimistic).await?;
            let conflicted = match work(tx.clone()).await {
                Ok(value) => match tx.commit().await {
                    Ok(()) => {
                        if run == 0 {
                            self.pool.note_first_run(false);
                        }
                        return Ok(value);
                    }
                    Err(e) if e.code == ErrorCode::Conflict => true,
                    Err(e) => return Err(e.into()),
                },
                Err(e) => {
                    let conflicted = tx.conflicted();
                    let _ = tx.rollback().await;
                    if !conflicted {
                        return Err(e);
                    }
                    true
                }
            };
            if run == 0 && conflicted {
                self.pool.note_first_run(true);
            }
        }
        self.exclusive(work).await
    }

    /// Runs the statements of `batch` as one transaction, in one request:
    /// all of them take effect or none does, nothing else is written
    /// between them, and they are on disk when this returns. Gives one
    /// result for each statement, in order.
    ///
    /// This is for a transaction whose statements need nothing from each
    /// other's answers. Where `transaction` talks to the server once per
    /// statement and may be run again after a conflict, a batch is one
    /// round trip and runs once, as the database's only writer for the
    /// moment it takes. A statement can still be made a condition of the
    /// rest: see `Batch::expect`. A batch of queries alone reads one
    /// moment of the database and holds nobody up.
    ///
    /// ```ignore
    /// db.batch(
    ///     Batch::new()
    ///         .add(Update::table("accounts").key([from]).filter(col("balance").ge(amount)).set("balance", col("balance") - amount))
    ///         .expect(1)
    ///         .add(Update::table("accounts").key([to]).set("balance", col("balance") + amount))
    ///         .expect(1)
    ///         .add(Insert::into("ledger").set("from_id", from).set("to_id", to).set("amount", amount)),
    /// ).await?;
    /// ```
    pub async fn batch(&self, batch: Batch) -> DbResult<Vec<Rows>> {
        let statements = batch.items.len();
        if statements == 0 {
            return Ok(Vec::new());
        }
        let mut conn = self.pool.acquire().await?;
        let answers = conn.conn().call_batch(Request::Batch(batch.items), statements).await?;
        answers.into_iter().map(rows).collect()
    }

    /// Runs `work`, once, in a transaction that is the database's only
    /// writer while it runs (see `begin`): commits if it returns `Ok`,
    /// rolls back if it returns `Err` or panics. It becomes the writer
    /// with its first statement, which is when it begins on the server.
    pub async fn exclusive<T, E, F, Fut>(&self, work: F) -> Result<T, E>
    where
        F: FnOnce(Transaction) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: From<DbError>,
    {
        let tx = self.begin_later(TxnKind::Exclusive).await?;
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
    /// of the database, that of its first statement.
    pub async fn read<T, E, F, Fut>(&self, work: F) -> Result<T, E>
    where
        F: FnOnce(Transaction) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: From<DbError>,
    {
        let tx = self.begin_later(TxnKind::Read).await?;
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

    /// The roles of this client's database: who else may connect to it,
    /// and what they may do. For a client that connected with the
    /// database's own password or the administrator's, not as a role.
    pub fn roles(&self) -> Roles<'_> {
        Roles { client: self }
    }
}

impl Db for Client {
    async fn call(&self, request: Request) -> DbResult<Response> {
        let mut conn = self.pool.acquire().await?;
        conn.conn().call(request).await
    }
}

struct TxState {
    /// `None` once the transaction has ended.
    conn: Option<PooledConn>,
    /// How to begin it, while it has not begun on the server: its first
    /// statement takes the begin along.
    pending: Option<(TxnKind, u64)>,
}

struct TxInner {
    state: Mutex<TxState>,
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
    fn on(conn: PooledConn, pending: Option<(TxnKind, u64)>) -> Transaction {
        Transaction { inner: Arc::new(TxInner { state: Mutex::new(TxState { conn: Some(conn), pending }), conflicted: AtomicBool::new(false) }) }
    }

    /// Makes the transaction's changes permanent and visible. When this
    /// returns `Ok` they are on disk.
    pub async fn commit(self) -> DbResult<()> {
        self.finish(Request::Commit).await
    }

    pub async fn rollback(self) -> DbResult<()> {
        self.finish(Request::Rollback).await
    }

    async fn finish(self, request: Request) -> DbResult<()> {
        let mut state = self.inner.state.lock().await;
        let mut conn = state.conn.take().ok_or_else(ended)?;
        if state.pending.take().is_some() {
            // It ran no statement, so it never began: there is nothing
            // on the server to end.
            return Ok(());
        }
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
        let mut state = self.inner.state.lock().await;
        let TxState { conn, pending } = &mut *state;
        let conn = conn.as_mut().ok_or_else(ended)?;
        let begin = pending.take();
        let request = match begin {
            // Its first statement: the server begins the transaction and
            // runs the statement in it. Whatever it answers, the
            // connection then carries a transaction that has to be
            // ended (see `Request::BeginWith`).
            Some((kind, lock_wait_ms)) => {
                conn.held = true;
                Request::BeginWith { kind, lock_wait_ms, statement: Box::new(request) }
            }
            None => request,
        };
        let result = conn.conn().call(request).await;
        if result.is_err() && begin.is_some() && conn.conn().never_left() {
            // Refused here, before it was sent (it was too large to
            // send): the server has begun nothing. The transaction is
            // still to begin, with whatever statement comes next; were
            // it taken for begun, that statement would go out alone.
            *pending = begin;
            conn.held = false;
        }
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

/// What a role of the database may do: one entry of `Roles::list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleRights {
    pub role: String,
    /// The table the rights are on; `None` for the rights the role holds
    /// on every table, which every role is listed with, held or not.
    pub table: Option<String>,
    pub rights: Rights,
}

/// The roles of a database. See `Client::roles`.
///
/// A role is a name with a password of its own and rights on tables. A
/// client connects as one by naming it (`Config::role`, `?role=` in the
/// URL) with the role's password, and each of its statements is then
/// refused unless the role holds the right for everything the statement
/// would read or change.
pub struct Roles<'a> {
    client: &'a Client,
}

impl Roles<'_> {
    async fn call(&self, request: AdminRequest) -> DbResult<Response> {
        self.client.call(Request::Admin(request)).await
    }

    /// Makes a role, which may do nothing until it is granted rights.
    pub async fn create(&self, name: &str, password: &str) -> DbResult<()> {
        ok(self.call(AdminRequest::CreateRole { name: name.into(), password: password.into(), if_not_exists: false }).await?)
    }

    /// Makes a role unless there is one of that name, which is left as
    /// it is, password and rights.
    pub async fn create_if_not_exists(&self, name: &str, password: &str) -> DbResult<()> {
        ok(self.call(AdminRequest::CreateRole { name: name.into(), password: password.into(), if_not_exists: true }).await?)
    }

    /// Removes a role and its rights. Its connections are refused from
    /// their next statement on.
    pub async fn drop(&self, name: &str) -> DbResult<()> {
        ok(self.call(AdminRequest::DropRole { name: name.into(), if_exists: false }).await?)
    }

    pub async fn drop_if_exists(&self, name: &str) -> DbResult<()> {
        ok(self.call(AdminRequest::DropRole { name: name.into(), if_exists: true }).await?)
    }

    /// Changes a role's password. Connections already open stay open.
    pub async fn set_password(&self, name: &str, password: &str) -> DbResult<()> {
        ok(self.call(AdminRequest::SetRolePassword { name: name.into(), password: password.into() }).await?)
    }

    /// Gives a role rights on one table.
    pub async fn grant(&self, role: &str, rights: Rights, table: &str) -> DbResult<()> {
        ok(self.call(AdminRequest::Grant { role: role.into(), table: Some(table.into()), rights }).await?)
    }

    /// Gives a role rights on every table of the database, those made
    /// later included. `Rights::SCHEMA` so given is also the right to
    /// create tables.
    pub async fn grant_everywhere(&self, role: &str, rights: Rights) -> DbResult<()> {
        ok(self.call(AdminRequest::Grant { role: role.into(), table: None, rights }).await?)
    }

    /// Takes rights on one table from a role. Fails, having changed
    /// nothing, if the role holds one of them on every table: such a
    /// right is taken with `revoke_everywhere`.
    pub async fn revoke(&self, role: &str, rights: Rights, table: &str) -> DbResult<()> {
        ok(self.call(AdminRequest::Revoke { role: role.into(), table: Some(table.into()), rights }).await?)
    }

    /// Takes rights from a role on every table, however they were
    /// granted. When this returns, no statement that begins runs by them.
    pub async fn revoke_everywhere(&self, role: &str, rights: Rights) -> DbResult<()> {
        ok(self.call(AdminRequest::Revoke { role: role.into(), table: None, rights }).await?)
    }

    /// Every role and what it may do, by role and then by table.
    pub async fn list(&self) -> DbResult<Vec<RoleRights>> {
        let Response::Rows { set, .. } = self.call(AdminRequest::ListRoles).await? else { return Err(unexpected("list roles")) };
        let mut listed = Vec::with_capacity(set.rows.len());
        for row in set.rows {
            let [Value::Text(role), table, Value::List(names)] = <[Value; 3]>::try_from(row).map_err(|_| unexpected("list roles"))? else { return Err(unexpected("list roles")) };
            let table = match table {
                Value::Text(table) => Some(table),
                _ => None,
            };
            let mut rights = Rights::NONE;
            for name in &names {
                let Value::Text(name) = name else { return Err(unexpected("list roles")) };
                rights = rights | Rights::parse(name)?;
            }
            listed.push(RoleRights { role, table, rights });
        }
        Ok(listed)
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
