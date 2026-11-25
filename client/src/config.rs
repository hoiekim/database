//! Where the server is and how to talk to it.

use std::path::PathBuf;
use std::time::Duration;

use database_protocol::error::{DbError, DbResult};

use crate::tls::{Tls, Trust};

pub const DEFAULT_PORT: u16 = 5454;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// A Unix domain socket the server listens on (its `DATABASE_SOCKET`).
    /// When set, connections go through it and `host` and `port` are not
    /// used: for a client on the server's own host, where it saves each
    /// request the trip through the network stack.
    pub socket: Option<PathBuf>,
    /// Whether connections over TCP are encrypted, and how the server is
    /// then known to be the server. `None` sends everything in the clear,
    /// the password included, which is right only on a network nobody
    /// else can listen on. A server may refuse such a connection (its
    /// `DATABASE_TLS`). A Unix domain socket is never encrypted, so a
    /// configuration with both this and `socket` is refused.
    pub tls: Option<Tls>,
    /// Empty for a connection that only administers the server.
    pub database: String,
    pub password: String,
    /// Connections kept at most. A transaction or a held lock occupies one
    /// until it ends.
    pub pool_size: usize,
    pub connect_timeout: Duration,
    /// How long the server may spend on one statement. Zero for no limit
    /// beyond the server's own.
    pub statement_timeout: Duration,
    /// How long a write waits for the database's write lock before it
    /// fails with `LockTimeout`.
    pub lock_wait: Duration,
    /// How many times `Client::transaction` runs its work beside other
    /// writers before it runs it as the database's only writer: 2 unless
    /// set. With 1 a transaction goes straight to the write lock after
    /// its first conflict, which is right where conflicts are the rule
    /// (many transactions changing the same row): a second run beside
    /// the others would only conflict again. With 0 every transaction
    /// takes the lock from the start.
    pub optimistic_runs: usize,
    /// Shown in the server's log.
    pub name: String,
}

impl Config {
    pub fn new(host: &str, port: u16, database: &str, password: &str) -> Config {
        Config {
            host: host.to_string(),
            port,
            socket: None,
            tls: None,
            database: database.to_string(),
            password: password.to_string(),
            pool_size: 8,
            connect_timeout: Duration::from_secs(10),
            statement_timeout: Duration::from_secs(60),
            lock_wait: Duration::from_secs(30),
            optimistic_runs: 2,
            name: String::new(),
        }
    }

    /// Where connections go, for messages.
    pub(crate) fn address(&self) -> String {
        match &self.socket {
            Some(path) => path.display().to_string(),
            None => format!("{}:{}", self.host, self.port),
        }
    }

    /// Reads `database://<database>:<password>@<host>[:<port>][/<database>]`
    /// with optional `?pool=8&connect_timeout=10&statement_timeout=60&lock_wait=30&name=app`
    /// (times in seconds), `&socket=/run/database/database.sock` to
    /// connect through the server's Unix domain socket instead of the
    /// host and port, and `&optimistic_runs=1` (see the field). `&tls=on`
    /// encrypts the connection and verifies the server against the
    /// system's certificate authorities; `&tls_ca=<file>` against the
    /// authorities in that file instead; `&tls_cert=<file>` by its being
    /// exactly that certificate; `&tls_name=<name>` names what the
    /// certificate must be for, where that is not the host. Each of the
    /// last three turns TLS on by itself, and `&tls=off` is the default:
    /// see `Config::tls`. The database may be named before the password or as the path; an
    /// administrator's URL names none. Percent-encoding is decoded in
    /// every part.
    pub fn from_url(url: &str) -> DbResult<Config> {
        let bad = |why: &str| DbError::invalid(format!("invalid database URL: {why}"));
        let rest = url.strip_prefix("database://").ok_or_else(|| bad("it must start with database://"))?;
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (userinfo, address) = rest.rsplit_once('@').ok_or_else(|| bad("no password before the host"))?;
        let (user, password) = userinfo.split_once(':').unwrap_or(("", userinfo));
        let (hostport, path) = address.split_once('/').unwrap_or((address, ""));
        let (host, port) = match hostport.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().map_err(|_| bad("the port is not a number"))?),
            None => (hostport, DEFAULT_PORT),
        };
        if host.is_empty() {
            return Err(bad("no host"));
        }
        let database = if path.is_empty() { user } else { path };
        let mut config = Config::new(host, port, &decode(database)?, &decode(password)?);
        let (mut on, mut trust, mut name) = (None, None, None);
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').ok_or_else(|| bad("a setting without a value"))?;
            let value = decode(value)?;
            let seconds = || value.parse::<f64>().ok().and_then(|s| Duration::try_from_secs_f64(s).ok()).ok_or_else(|| bad("a time that is not a number of seconds"));
            match key {
                "pool" => config.pool_size = value.parse().ok().filter(|n| *n > 0).ok_or_else(|| bad("pool must be a positive number"))?,
                "connect_timeout" => config.connect_timeout = seconds()?,
                "statement_timeout" => config.statement_timeout = seconds()?,
                "lock_wait" => config.lock_wait = seconds()?,
                "optimistic_runs" => config.optimistic_runs = value.parse().map_err(|_| bad("optimistic_runs must be a number"))?,
                "socket" => {
                    if value.is_empty() {
                        return Err(bad("socket must be a path"));
                    }
                    config.socket = Some(PathBuf::from(value));
                }
                "name" => config.name = value,
                "tls" => {
                    on = Some(match value.as_str() {
                        "on" => true,
                        "off" => false,
                        _ => return Err(bad("tls must be on or off")),
                    })
                }
                "tls_ca" | "tls_cert" => {
                    if value.is_empty() || trust.is_some() {
                        return Err(bad("one of tls_ca and tls_cert, which must be a path"));
                    }
                    trust = Some(if key == "tls_ca" { Trust::Authority(PathBuf::from(value)) } else { Trust::Certificate(PathBuf::from(value)) });
                }
                "tls_name" => {
                    if value.is_empty() {
                        return Err(bad("tls_name must be a name"));
                    }
                    name = Some(value);
                }
                _ => return Err(bad("an unknown setting")),
            }
        }
        // A setting that says how to verify the server is not one to
        // drop quietly: with `tls=off` beside it, the URL contradicts
        // itself.
        match (on, trust.is_some() || name.is_some()) {
            (Some(false), true) => return Err(bad("tls=off beside a setting that only TLS uses")),
            (Some(true), _) | (None, true) => config.tls = Some(Tls { trust: trust.unwrap_or(Trust::System), name }),
            _ => {}
        }
        if config.tls.is_some() && config.socket.is_some() {
            return Err(bad("a Unix domain socket is not encrypted: give socket or TLS, not both"));
        }
        Ok(config)
    }
}

fn decode(text: &str) -> DbResult<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok());
            out.push(hex.ok_or_else(|| DbError::invalid("invalid database URL: a bad % escape"))?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| DbError::invalid("invalid database URL: not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_url() {
        let c = Config::from_url("database://shop:s3cr%40t@db:6000?pool=3&statement_timeout=1.5&name=shop-1").unwrap();
        assert_eq!((c.host.as_str(), c.port, c.database.as_str(), c.password.as_str()), ("db", 6000, "shop", "s3cr@t"));
        assert_eq!((c.pool_size, c.statement_timeout, c.name.as_str()), (3, Duration::from_millis(1500), "shop-1"));

        let c = Config::from_url("database://anything:pw@localhost/blog").unwrap();
        assert_eq!((c.port, c.database.as_str()), (DEFAULT_PORT, "blog"));

        let admin = Config::from_url("database://adminpw@localhost").unwrap();
        assert_eq!((admin.database.as_str(), admin.password.as_str()), ("", "adminpw"));
        assert_eq!((admin.socket.clone(), admin.optimistic_runs), (None, 2));

        let local = Config::from_url("database://shop:pw@localhost?socket=%2Frun%2Fdatabase%2Fdatabase.sock&optimistic_runs=1").unwrap();
        assert_eq!(local.socket.as_deref(), Some(std::path::Path::new("/run/database/database.sock")));
        assert_eq!((local.optimistic_runs, local.address().as_str()), (1, "/run/database/database.sock"));

        let plain = Config::from_url("database://shop:pw@db?tls=off").unwrap();
        assert_eq!(plain.tls, None);
        let system = Config::from_url("database://shop:pw@db?tls=on").unwrap();
        assert_eq!(system.tls, Some(Tls::system()));
        let authority = Config::from_url("database://shop:pw@10.0.0.5?tls_ca=%2Fetc%2Fdatabase%2Fca.pem&tls_name=db.example.com").unwrap();
        assert_eq!(authority.tls, Some(Tls::authority("/etc/database/ca.pem").name("db.example.com")));
        let exactly = Config::from_url("database://shop:pw@db?tls=on&tls_cert=server.pem").unwrap();
        assert_eq!(exactly.tls, Some(Tls::certificate("server.pem")));
        for bad in ["database://a:b@h/d?tls=yes", "database://a:b@h/d?tls=off&tls_ca=ca.pem", "database://a:b@h/d?tls=off&tls_name=db", "database://a:b@h/d?tls_ca=", "database://a:b@h/d?tls_ca=a&tls_cert=b", "database://a:b@h/d?tls_name=", "database://a:b@h/d?tls=on&socket=%2Frun%2Fdb.sock"] {
            assert!(Config::from_url(bad).is_err(), "{bad}");
        }

        for bad in ["database://a:b@h/d?statement_timeout=-1", "database://a:b@h/d?lock_wait=NaN", "database://a:b@h/d?connect_timeout=1e400", "postgres://a:b@h/d", "database://nohost", "database://a:b@:1/d", "database://a:b@h:x/d", "database://a:b@h/d?pool=0", "database://a:b@h/d?what=1", "database://a:%zz@h/d", "database://a:b@h/d?socket=", "database://a:b@h/d?optimistic_runs=two"] {
            assert!(Config::from_url(bad).is_err(), "{bad}");
        }
    }
}
