//! TLS on the TCP port: a client knows the server before it sends its
//! password, a server that requires TLS serves nothing in the clear, and
//! a certificate is renewed without a restart. R-O8 to R-O10.

mod common;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::*;
use database_protocol::message::{decode_response, encode_request, Request, Response, PROTOCOL_VERSION};
use database_server::TlsConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The certificates of `protocol/fixtures/tls`: `server` and `renewed`
/// are signed by `ca`, `stranger` by `stranger-ca`, and all three are for
/// `localhost`, `database.test` and the loopback addresses.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../protocol/fixtures/tls").join(name)
}

/// A server with the `server` certificate, taking TCP in the clear too
/// if `plain`, and a database named `test` on it.
async fn tls_server(plain: bool) -> TestServer {
    let server = TestServer::start_with(TempDir::new(), |config| config.tls = Some(TlsConfig { certificate: fixture("server.pem"), key: fixture("server.key"), plain })).await;
    admin(&server).await.admin().create_database("test", PASSWORD).await.expect("database is created");
    server
}

fn admin_config(server: &TestServer) -> Config {
    let mut config = Config::new("localhost", server.port, "", ADMIN_PASSWORD);
    config.tls = Some(Tls::authority(fixture("ca.pem")));
    config
}

async fn admin(server: &TestServer) -> Client {
    Client::connect(admin_config(server)).await.expect("admin connects over TLS")
}

/// How a client on `test` connects: to `localhost`, with no TLS until
/// the test says which.
fn config(server: &TestServer) -> Config {
    Config::new("localhost", server.port, "test", PASSWORD)
}

async fn connect(server: &TestServer, tls: Tls) -> DbResult<Client> {
    let mut config = config(server);
    config.tls = Some(tls);
    Client::connect(config).await
}

async fn notes(db: &Client) -> DbResult<()> {
    db.create_table(TableDef::new("notes").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("body", Type::Text)).primary_key(["id"])).await
}

// R-O8: everything a connection does, it does under TLS.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_verifies_the_server_reads_and_writes_over_tls() -> DbResult<()> {
    let server = tls_server(false).await;
    let url = format!("database://test:{PASSWORD}@localhost:{}?tls_ca={}", server.port, fixture("ca.pem").display());
    let db = Client::connect_url(&url).await?;
    notes(&db).await?;
    // Results of many frames, each far larger than a TLS record, and
    // requests as large.
    for id in 0..12 {
        db.insert(Insert::into("notes").set("id", id).set("body", "y".repeat(1_000_000))).await?;
    }
    let rows = db.query(Query::table("notes")).await?;
    assert_eq!(rows.len(), 12);
    assert!(rows.iter().all(|row| row.get::<String>("body").unwrap().len() == 1_000_000));
    db.transaction(|tx| async move {
        let body: String = tx.query(Query::table("notes").key([3])).await?.one()?.get("body")?;
        tx.update(Update::table("notes").key([3]).set("body", format!("{}!", &body[..4]))).await?;
        Ok::<_, DbError>(())
    })
    .await?;
    let by_id = Prepared::new(Query::table("notes").key([param(0)]));
    assert_eq!(db.execute(&by_id, [3]).await?.one()?.get::<String>("body")?, "yyyy!");
    let guard = db.lock("job").await?;
    assert!(db.try_lock("job").await?.is_none());
    guard.unlock().await?;
    // A dump is streamed to the client as it is made.
    let mut dump = Vec::new();
    let bytes = admin(&server).await.admin().dump(&["test"], &mut dump).await?;
    assert!(bytes > 11_000_000 && dump.len() as u64 == bytes);
    // The certificate carries the loopback address too, and a name the
    // host is not reached by can be given.
    let mut by_address = config(&server);
    by_address.host = "127.0.0.1".to_string();
    by_address.tls = Some(Tls::authority(fixture("ca.pem")));
    assert_eq!(Client::connect(by_address.clone()).await?.query(Query::table("notes").key([3])).await?.len(), 1);
    by_address.tls = Some(Tls::authority(fixture("ca.pem")).name("database.test"));
    assert_eq!(Client::connect(by_address).await?.query(Query::table("notes").key([3])).await?.len(), 1);
    Ok(())
}

/// Sends a greeting in the clear and returns what comes back: the
/// answer, and whether the server then closed the connection.
async fn greet_in_the_clear(port: u16, password: &str) -> (Response, bool) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let hello = encode_request(&Request::Hello { version: PROTOCOL_VERSION, database: "test".into(), password: password.into(), client: "raw".into() }, 0);
    stream.write_all(&(hello.len() as u32).to_le_bytes()).await.unwrap();
    stream.write_all(&hello).await.unwrap();
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await.unwrap();
    let mut frame = vec![0u8; u32::from_le_bytes(len) as usize];
    stream.read_exact(&mut frame).await.unwrap();
    // A request sent after the answer: served, or met by the end.
    let ping = encode_request(&Request::Ping, 0);
    let _ = stream.write_all(&(ping.len() as u32).to_le_bytes()).await;
    let _ = stream.write_all(&ping).await;
    let closed = !matches!(tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut len)).await, Ok(Ok(_)));
    (decode_response(&frame).unwrap(), closed)
}

// R-O9: with TLS required, a greeting in the clear is refused whatever
// password it carries, in a frame the client can read, and nothing is
// served after it.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_requires_tls_refuses_a_greeting_in_the_clear_and_says_why() -> DbResult<()> {
    let server = tls_server(false).await;
    for password in [PASSWORD, ADMIN_PASSWORD, "wrong"] {
        let (answer, closed) = greet_in_the_clear(server.port, password).await;
        let Response::Error(e) = answer else { panic!("a greeting in the clear was answered {answer:?}") };
        assert_eq!(e.code, ErrorCode::AccessDenied);
        assert!(e.message.contains("TLS"), "{}", e.message);
        assert!(closed, "the connection is closed after the refusal");
    }
    // The client reports the same.
    let refused = Client::connect(config(&server)).await.err().expect("a client in the clear is refused");
    assert_eq!(refused.code, ErrorCode::AccessDenied);
    assert!(refused.message.contains("TLS"), "{}", refused.message);
    // And the same client under TLS is served.
    connect(&server, Tls::authority(fixture("ca.pem"))).await?.ping().await
}

// R-O9: where TLS is optional, one port serves both, and both see the
// same database.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_where_tls_is_optional_serves_both_on_one_port() -> DbResult<()> {
    let server = tls_server(true).await;
    let plain = Client::connect(config(&server)).await?;
    let secure = connect(&server, Tls::authority(fixture("ca.pem"))).await?;
    notes(&plain).await?;
    secure.insert(Insert::into("notes").set("id", 1).set("body", "over TLS")).await?;
    assert_eq!(plain.query(Query::table("notes").key([1])).await?.one()?.get::<String>("body")?, "over TLS");
    let (answer, closed) = greet_in_the_clear(server.port, PASSWORD).await;
    assert!(matches!(answer, Response::Hello { .. }) && !closed);
    // A wrong password is a wrong password either way.
    let mut wrong = config(&server);
    wrong.password = "wrong".to_string();
    assert_eq!(Client::connect(wrong.clone()).await.err().map(|e| e.code), Some(ErrorCode::AccessDenied));
    wrong.tls = Some(Tls::authority(fixture("ca.pem")));
    assert_eq!(Client::connect(wrong).await.err().map(|e| e.code), Some(ErrorCode::AccessDenied));
    Ok(())
}

// R-O8: a server the client cannot verify is not sent a greeting.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_refuses_a_server_it_cannot_verify() -> DbResult<()> {
    let server = tls_server(true).await;
    let refused = |tls: Tls, why: &'static str| {
        let server = &server;
        async move {
            let e = connect(server, tls).await.err().unwrap_or_else(|| panic!("connected to a server with {why}"));
            assert_eq!(e.code, ErrorCode::Connection, "{why}: {}", e.message);
            assert!(e.message.contains("TLS handshake"), "{why}: {}", e.message);
        }
    };
    refused(Tls::authority(fixture("stranger-ca.pem")), "a certificate from an authority the client was not given").await;
    // The system's authorities, wherever it keeps them, do not include
    // the one these tests made.
    assert_eq!(connect(&server, Tls::system()).await.err().map(|e| e.code), Some(ErrorCode::Connection));
    refused(Tls::authority(fixture("ca.pem")).name("elsewhere.test"), "a certificate for another name").await;
    // A file that holds no authority at all is not an empty list of them.
    let e = connect(&server, Tls::authority(fixture("server.key"))).await.err().expect("a key is not a certificate");
    assert!(e.message.contains("holds no certificate"), "{}", e.message);
    // The server is still there for a client that can verify it.
    connect(&server, Tls::authority(fixture("ca.pem"))).await?.ping().await
}

// R-O8: a client told to use TLS never talks in the clear instead.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_asks_for_tls_does_not_fall_back_to_a_server_in_the_clear() -> DbResult<()> {
    let (server, plain) = fresh().await;
    plain.ping().await?;
    let mut config = Config::from_url(&server.url("test"))?;
    config.tls = Some(Tls::authority(fixture("ca.pem")));
    let e = Client::connect(config.clone()).await.err().expect("no TLS, no connection");
    assert_eq!(e.code, ErrorCode::Connection);
    assert!(e.message.contains("does not serve TLS"), "{}", e.message);
    // Nor through a socket, which is never encrypted.
    config.socket = Some(server.dir.path().join("database.sock"));
    let e = Client::connect(config).await.err().expect("a socket and TLS together are refused");
    assert_eq!(e.code, ErrorCode::InvalidRequest);
    Ok(())
}

// R-O8: a client given the server's certificate takes that one and no
// other, whoever signed the other and whatever name it carries.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_given_the_certificate_takes_exactly_that_one() -> DbResult<()> {
    let server = tls_server(false).await;
    let mut by_address = config(&server);
    // No name is compared: the address is not asked to be in it.
    by_address.host = "127.0.0.1".to_string();
    by_address.tls = Some(Tls::certificate(fixture("server.pem")).name("not-in-the-certificate.test"));
    Client::connect(by_address).await?.ping().await?;
    for other in ["renewed.pem", "stranger.pem", "ca.pem"] {
        let e = connect(&server, Tls::certificate(fixture(other))).await.err().unwrap_or_else(|| panic!("the server's certificate was taken for {other}"));
        assert_eq!(e.code, ErrorCode::Connection, "{other}: {}", e.message);
    }
    Ok(())
}

fn copy(from: &str, to: &Path) {
    // Written beside and renamed, as a renewal should: nobody reads half
    // a file.
    let temp = to.with_extension("new");
    std::fs::copy(fixture(from), &temp).unwrap();
    std::fs::rename(&temp, to).unwrap();
}

/// Waits until the server shows the certificate of `file`.
async fn shows(server: &TestServer, file: &str) -> bool {
    for _ in 0..100 {
        if connect(server, Tls::certificate(fixture(file))).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

// R-O10: a certificate replaced on disk is the one new connections are
// shown, with no restart; connections already open carry on; and files
// that cannot be used leave the server on the certificate it has.
#[tokio::test(flavor = "multi_thread")]
async fn a_renewed_certificate_is_served_without_a_restart() -> DbResult<()> {
    let dir = TempDir::new();
    let (certificate, key) = (dir.path().join("tls.pem"), dir.path().join("tls.key"));
    copy("server.pem", &certificate);
    copy("server.key", &key);
    let server = TestServer::start_with(dir, |config| config.tls = Some(TlsConfig::new(&certificate, &key))).await;
    admin(&server).await.admin().create_database("test", PASSWORD).await?;
    let before = connect(&server, Tls::certificate(fixture("server.pem"))).await?;
    notes(&before).await?;

    // Half a renewal: the new certificate beside the old key. The two do
    // not belong together, and the server keeps what it has.
    copy("renewed.pem", &certificate);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    connect(&server, Tls::authority(fixture("ca.pem"))).await?.ping().await?;
    assert!(shows(&server, "server.pem").await, "the certificate from before is still served");

    // The whole of it.
    copy("renewed.key", &key);
    assert!(shows(&server, "renewed.pem").await, "the renewed certificate is served");
    assert!(connect(&server, Tls::certificate(fixture("server.pem"))).await.is_err(), "and the one before it no longer");
    // A connection from before the renewal is not disturbed.
    before.insert(Insert::into("notes").set("id", 1).set("body", "still here")).await?;

    // Files that hold no certificate at all change nothing either.
    std::fs::write(&certificate, "not a certificate").unwrap();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let after = connect(&server, Tls::certificate(fixture("renewed.pem"))).await?;
    assert_eq!(after.query(Query::table("notes")).await?.len(), 1);
    Ok(())
}

/// The processor time this process has used, in seconds.
#[cfg(target_os = "linux")]
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let fields: Vec<&str> = stat.rsplit_once(')').unwrap().1.split_whitespace().collect();
    // utime and stime, in ticks of a hundredth of a second.
    (fields[11].parse::<f64>().unwrap() + fields[12].parse::<f64>().unwrap()) / 100.0
}

// R-O9: a connection that has not sent enough to tell what it is costs
// the server no processor time while it waits, is closed when its time
// to greet is over, and keeps no other connection waiting.
#[tokio::test(flavor = "multi_thread")]
async fn a_connection_that_sends_too_little_to_tell_what_it_is_waits_idle_and_is_closed() -> DbResult<()> {
    let server = tls_server(true).await;
    let started = std::time::Instant::now();
    // Each of these is the start of a handshake, or of a greeting, cut
    // short: one byte, two, and one byte before the peer stops sending.
    let mut held = Vec::new();
    for n in 0..300 {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", server.port)).await.unwrap();
        stream.write_all(if n % 3 == 1 { &[0x16, 0x03] } else { &[0x16] }).await.unwrap();
        if n % 3 == 2 {
            stream.shutdown().await.unwrap();
        }
        held.push(stream);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    #[cfg(target_os = "linux")]
    {
        let before = cpu_seconds();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let used = cpu_seconds() - before;
        assert!(used < 0.5, "three hundred silent connections cost {used} s of processor time in 3 s");
    }
    // Others are served meanwhile, in the clear and under TLS.
    Client::connect(config(&server)).await?.ping().await?;
    connect(&server, Tls::authority(fixture("ca.pem"))).await?.ping().await?;
    // And each is closed when the ten seconds a connection has to say
    // what it is are over: the server sends nothing and ends it.
    for mut stream in held {
        let mut byte = [0u8; 1];
        let ended = tokio::time::timeout_at((started + Duration::from_secs(20)).into(), stream.read(&mut byte)).await;
        assert!(matches!(ended, Ok(Ok(0)) | Ok(Err(_))), "a silent connection was still open, or was answered: {ended:?}");
    }
    assert!(started.elapsed() >= Duration::from_secs(9), "they were closed before their time");
    Ok(())
}

// R-O9: a greeting in the clear whose length begins with the byte a
// handshake begins with is a greeting all the same.
#[tokio::test(flavor = "multi_thread")]
async fn a_greeting_whose_first_byte_is_a_handshakes_is_served_as_a_greeting() -> DbResult<()> {
    for plain in [true, false] {
        let server = tls_server(plain).await;
        // Frames of 278 and of 534 bytes: their lengths begin 16 01 and 16 02.
        for len in [0x116usize, 0x216] {
            let hello = |client: String| encode_request(&Request::Hello { version: PROTOCOL_VERSION, database: "test".into(), password: PASSWORD.into(), client }, 0);
            let pad = len - hello(String::new()).len();
            // A name of 128 bytes and more takes a byte more to say how long it is.
            let frame = [pad, pad - 1].into_iter().map(|n| hello("x".repeat(n))).find(|frame| frame.len() == len).expect("a greeting of that length");
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", server.port)).await.unwrap();
            stream.write_all(&(frame.len() as u32).to_le_bytes()).await.unwrap();
            stream.write_all(&frame).await.unwrap();
            let mut len = [0u8; 4];
            stream.read_exact(&mut len).await.unwrap();
            let mut answer = vec![0u8; u32::from_le_bytes(len) as usize];
            stream.read_exact(&mut answer).await.unwrap();
            match decode_response(&answer).unwrap() {
                Response::Hello { .. } if plain => {}
                Response::Error(e) if !plain && e.code == ErrorCode::AccessDenied && e.message.contains("TLS") => {}
                other => panic!("a greeting of {} bytes, in the clear allowed: {plain}, was answered {other:?}", frame.len()),
            }
        }
    }
    Ok(())
}

// R-O9: a server asked for TLS that cannot serve it does not start.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_does_not_start_on_a_certificate_it_cannot_use() {
    let start = |certificate: PathBuf, key: PathBuf| async move {
        let dir = TempDir::new();
        let mut config = database_server::ServerConfig::new(dir.path(), ADMIN_PASSWORD);
        config.tls = Some(TlsConfig::new(certificate, key));
        database_server::Server::start(config).await.err().map(|e| e.to_string())
    };
    let mismatched = start(fixture("server.pem"), fixture("renewed.key")).await.expect("a key that is not the certificate's");
    assert!(mismatched.contains("TLS certificate cannot be used"), "{mismatched}");
    assert!(start(fixture("missing.pem"), fixture("server.key")).await.is_some(), "a certificate that is not there");
    assert!(start(fixture("server.key"), fixture("server.key")).await.is_some(), "a file with no certificate in it");
    assert!(start(fixture("server.pem"), fixture("server.pem")).await.is_some(), "a file with no key in it");
}

// R-O6 beside R-O9: the Unix domain socket never leaves the host and is
// served as it was, where TCP requires TLS.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_unix_socket_is_served_in_the_clear_where_tcp_requires_tls() -> DbResult<()> {
    let dir = TempDir::new();
    let path = dir.path().join("database.sock");
    let socket = path.clone();
    let server = TestServer::start_with(dir, |config| {
        config.socket = Some(socket);
        config.tls = Some(TlsConfig::new(fixture("server.pem"), fixture("server.key")));
    })
    .await;
    let mut local = Config::new("localhost", server.port, "", ADMIN_PASSWORD);
    local.socket = Some(path.clone());
    Client::connect(local.clone()).await?.admin().create_database("test", PASSWORD).await?;
    local.database = "test".to_string();
    local.password = PASSWORD.to_string();
    let db = Client::connect(local).await?;
    notes(&db).await?;
    assert_eq!(Client::connect(config(&server)).await.err().map(|e| e.code), Some(ErrorCode::AccessDenied), "TCP in the clear is still refused");
    let remote = connect(&server, Tls::authority(fixture("ca.pem"))).await?;
    assert_eq!(remote.describe().await?.len(), 1);
    Ok(())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

// R-O8: the server's own commands reach it under TLS with nothing but
// the settings it was started with, and its settings are held to.
#[test]
fn the_command_reaches_a_server_that_requires_tls_and_refuses_half_a_setting() {
    let dir = TempDir::new();
    let port = free_port();
    let command = |tls: &[(&str, PathBuf)], arg: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_database"));
        command.arg(arg).env("DATABASE_DATA", dir.path()).env("DATABASE_LISTEN", format!("127.0.0.1:{port}")).env("DATABASE_ADMIN_PASSWORD", ADMIN_PASSWORD);
        command.env_remove("DATABASE_SOCKET").env_remove("DATABASE_TLS").env_remove("DATABASE_TLS_CERT").env_remove("DATABASE_TLS_KEY");
        command.envs(tls.iter().map(|(name, value)| (name.to_string(), value.clone()))).stdout(Stdio::null()).stderr(Stdio::piped());
        command
    };
    // A setting that asks for TLS without the means stops the start.
    for half in [vec![("DATABASE_TLS_CERT", fixture("server.pem"))], vec![("DATABASE_TLS_KEY", fixture("server.key"))], vec![("DATABASE_TLS", PathBuf::from("required"))], vec![("DATABASE_TLS", PathBuf::from("sometimes")), ("DATABASE_TLS_CERT", fixture("server.pem")), ("DATABASE_TLS_KEY", fixture("server.key"))]] {
        let output = command(&half, "serve").output().unwrap();
        assert!(!output.status.success(), "started with {half:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("DATABASE_TLS"), "{half:?}");
    }
    let tls = [("DATABASE_TLS_CERT", fixture("server.pem")), ("DATABASE_TLS_KEY", fixture("server.key"))];
    let mut server = command(&tls, "serve").stderr(Stdio::null()).spawn().unwrap();
    let mut answered = false;
    for _ in 0..200 {
        if command(&tls, "ping").status().unwrap().success() {
            answered = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Without the certificate the command greets in the clear, and is
    // told what the server requires.
    let plain = command(&[], "ping").output().unwrap();
    let _ = server.kill();
    let _ = server.wait();
    assert!(answered, "`database ping` reached the server under TLS");
    assert!(!plain.status.success());
    assert!(String::from_utf8_lossy(&plain.stderr).contains("only under TLS"), "{}", String::from_utf8_lossy(&plain.stderr));
}
