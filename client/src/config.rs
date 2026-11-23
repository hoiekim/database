//! Where the server is and how to talk to it.

use std::time::Duration;

use database_protocol::error::{DbError, DbResult};

pub const DEFAULT_PORT: u16 = 5454;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
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
    /// Shown in the server's log.
    pub name: String,
}

impl Config {
    pub fn new(host: &str, port: u16, database: &str, password: &str) -> Config {
        Config {
            host: host.to_string(),
            port,
            database: database.to_string(),
            password: password.to_string(),
            pool_size: 8,
            connect_timeout: Duration::from_secs(10),
            statement_timeout: Duration::from_secs(60),
            lock_wait: Duration::from_secs(30),
            name: String::new(),
        }
    }

    /// Reads `database://<database>:<password>@<host>[:<port>][/<database>]`
    /// with optional `?pool=8&connect_timeout=10&statement_timeout=60&lock_wait=30&name=app`
    /// (times in seconds). The database may be named before the password
    /// or as the path; an administrator's URL names none. Percent-encoding
    /// is decoded in every part.
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
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').ok_or_else(|| bad("a setting without a value"))?;
            let value = decode(value)?;
            let seconds = || value.parse::<f64>().map(Duration::from_secs_f64).map_err(|_| bad("a time that is not a number"));
            match key {
                "pool" => config.pool_size = value.parse().ok().filter(|n| *n > 0).ok_or_else(|| bad("pool must be a positive number"))?,
                "connect_timeout" => config.connect_timeout = seconds()?,
                "statement_timeout" => config.statement_timeout = seconds()?,
                "lock_wait" => config.lock_wait = seconds()?,
                "name" => config.name = value,
                _ => return Err(bad("an unknown setting")),
            }
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
        let c = Config::from_url("database://mail:s3cr%40t@db:6000?pool=3&statement_timeout=1.5&name=mail-1").unwrap();
        assert_eq!((c.host.as_str(), c.port, c.database.as_str(), c.password.as_str()), ("db", 6000, "mail", "s3cr@t"));
        assert_eq!((c.pool_size, c.statement_timeout, c.name.as_str()), (3, Duration::from_millis(1500), "mail-1"));

        let c = Config::from_url("database://anything:pw@localhost/budget").unwrap();
        assert_eq!((c.port, c.database.as_str()), (DEFAULT_PORT, "budget"));

        let admin = Config::from_url("database://adminpw@localhost").unwrap();
        assert_eq!((admin.database.as_str(), admin.password.as_str()), ("", "adminpw"));

        for bad in ["postgres://a:b@h/d", "database://nohost", "database://a:b@:1/d", "database://a:b@h:x/d", "database://a:b@h/d?pool=0", "database://a:b@h/d?what=1", "database://a:%zz@h/d"] {
            assert!(Config::from_url(bad).is_err(), "{bad}");
        }
    }
}
