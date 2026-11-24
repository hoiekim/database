//! Just enough HTTP/1.1 to send JSON to a server on this machine and
//! read its JSON back over one connection that is kept open: what rqlite
//! and libSQL's server are spoken to with.

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

pub struct Http {
    /// `host:port`.
    address: String,
    stream: Option<BufReader<TcpStream>>,
    /// How long one request may take, answer included.
    patience: Duration,
}

/// The largest answer taken: far more than any the benchmark asks for.
const LARGEST_BODY: usize = 64 * 1024 * 1024;

impl Http {
    pub fn new(address: &str, patience: Duration) -> Http {
        Http { address: address.to_string(), stream: None, patience }
    }

    /// Closes the connection. The next request opens a new one.
    pub fn close(&mut self) {
        self.stream = None;
    }

    /// Sends a request and returns the status and the body of the answer.
    pub async fn request(&mut self, method: &str, path: &str, body: Option<&[u8]>) -> Result<(u16, Vec<u8>), String> {
        match tokio::time::timeout(self.patience, self.exchange(method, path, body)).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(e)) => {
                self.stream = None;
                Err(format!("{method} {path}: {e}"))
            }
            Err(_) => {
                self.stream = None;
                Err(format!("{method} {path}: no answer within {:?}", self.patience))
            }
        }
    }

    /// Posts JSON and reads JSON. Any status but 200 is an error that
    /// carries the body.
    pub async fn post(&mut self, path: &str, body: &Value) -> Result<Value, String> {
        let (status, answer) = self.request("POST", path, Some(body.to_string().as_bytes())).await?;
        if status != 200 {
            return Err(format!("POST {path}: status {status}: {}", String::from_utf8_lossy(&answer)));
        }
        serde_json::from_slice(&answer).map_err(|e| format!("POST {path}: the answer is not JSON: {e}"))
    }

    async fn exchange(&mut self, method: &str, path: &str, body: Option<&[u8]>) -> std::io::Result<(u16, Vec<u8>)> {
        let bad = |what: &str| std::io::Error::other(what.to_string());
        if self.stream.is_none() {
            let stream = TcpStream::connect(&self.address).await?;
            // Each request waits for its answer: never hold one back.
            stream.set_nodelay(true)?;
            self.stream = Some(BufReader::new(stream));
        }
        let stream = self.stream.as_mut().expect("opened above");
        let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\n", self.address).into_bytes();
        if let Some(body) = body {
            request.extend_from_slice(format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()).as_bytes());
        }
        request.extend_from_slice(b"\r\n");
        request.extend_from_slice(body.unwrap_or_default());
        stream.get_mut().write_all(&request).await?;

        let mut line = String::new();
        let (status, length, chunked, close) = loop {
            line.clear();
            if stream.read_line(&mut line).await? == 0 {
                return Err(bad("the server closed the connection"));
            }
            let status: u16 = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| bad("not an HTTP answer"))?;
            let (mut length, mut chunked, mut close) = (None, false, false);
            loop {
                line.clear();
                if stream.read_line(&mut line).await? == 0 {
                    return Err(bad("the server closed the connection inside the headers"));
                }
                let header = line.trim_end();
                if header.is_empty() {
                    break;
                }
                let Some((name, value)) = header.split_once(':') else { continue };
                let value = value.trim();
                match name.to_ascii_lowercase().as_str() {
                    "content-length" => length = Some(value.parse::<usize>().map_err(|_| bad("a content length that is not a number"))?),
                    "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
                    "connection" => close = value.eq_ignore_ascii_case("close"),
                    _ => {}
                }
            }
            // "100 Continue" and its like come before the answer proper.
            if (100..200).contains(&status) {
                continue;
            }
            break (status, length, chunked, close);
        };
        let mut answer = Vec::new();
        if chunked {
            loop {
                line.clear();
                stream.read_line(&mut line).await?;
                let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or_default(), 16).map_err(|_| bad("a chunk without a size"))?;
                if answer.len() + size > LARGEST_BODY {
                    return Err(bad("an answer larger than allowed"));
                }
                if size == 0 {
                    // Trailers, if any, up to the empty line.
                    loop {
                        line.clear();
                        if stream.read_line(&mut line).await? == 0 || line.trim_end().is_empty() {
                            break;
                        }
                    }
                    break;
                }
                let at = answer.len();
                answer.resize(at + size, 0);
                stream.read_exact(&mut answer[at..]).await?;
                line.clear();
                stream.read_line(&mut line).await?;
            }
        } else if let Some(length) = length {
            if length > LARGEST_BODY {
                return Err(bad("an answer larger than allowed"));
            }
            answer.resize(length, 0);
            stream.read_exact(&mut answer).await?;
        } else if status != 204 && status != 304 {
            // Neither a length nor chunks: the body ends with the connection.
            stream.take(LARGEST_BODY as u64).read_to_end(&mut answer).await?;
            self.stream = None;
        }
        if close {
            self.stream = None;
        }
        Ok((status, answer))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// A server for one connection: answers each request on it with the
    /// next of `answers`, and gives back what it was sent, head and body.
    pub async fn serve(listener: TcpListener, answers: Vec<String>) -> Vec<String> {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut seen = Vec::new();
        for answer in answers {
            let (mut head, mut length) = (String::new(), 0);
            loop {
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            seen.push(format!("{head}{}", String::from_utf8(body).unwrap()));
            stream.get_mut().write_all(answer.as_bytes()).await.unwrap();
        }
        seen
    }

    #[tokio::test]
    async fn requests_share_a_connection_and_bodies_come_back_whole() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(serve(
            listener,
            [
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"a\":[1,2]}",
                // What Go's server sends for an answer it did not measure.
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n6\r\n{\"b\": \r\n5\r\ntrue}\r\n0\r\n\r\n",
                "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\n\r\nbusy",
            ]
            .map(String::from)
            .to_vec(),
        ));
        let mut http = Http::new(&address, Duration::from_secs(5));
        assert_eq!(http.post("/one", &json!({"q": 1})).await.unwrap(), json!({"a": [1, 2]}));
        assert_eq!(http.post("/two", &json!(null)).await.unwrap(), json!({"b": true}));
        let refused = http.post("/three", &json!([])).await.unwrap_err();
        assert!(refused.contains("503") && refused.contains("busy"), "{refused}");
        let seen = server.await.unwrap();
        assert!(seen[0].starts_with("POST /one HTTP/1.1\r\n") && seen[0].ends_with("{\"q\":1}"), "{}", seen[0]);
        assert!(seen[0].contains(&format!("Host: {address}\r\n")) && seen[0].contains("Content-Length: 7\r\n"));
        assert!(seen[1].starts_with("POST /two "));
    }

    #[tokio::test]
    async fn a_server_that_goes_silent_is_given_up_on() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let held = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(stream);
        });
        let mut http = Http::new(&address, Duration::from_millis(100));
        let silent = http.request("GET", "/health", None).await.unwrap_err();
        assert!(silent.contains("no answer within"), "{silent}");
        held.abort();
    }
}
