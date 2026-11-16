//! Test-only: a tiny in-process HTTP server that records every request and answers from a
//! closure, so request shapes (paths, headers, bodies) can be checked without Graph. The server
//! sits on its own OS threads; nothing here is compiled into a release build.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

/// CR LF CR LF and CR LF as bytes, so no editor can turn them into plain newlines.
const HEADER_END: [u8; 4] = [13, 10, 13, 10];
const CRLF: [u8; 2] = [13, 10];

#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    /// Path and query as sent.
    pub target: String,
    /// Header names in lowercase.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    pub fn query(&self) -> &str {
        self.target.split_once('?').map_or("", |(_, q)| q)
    }

    /// The decoded value of one query parameter.
    pub fn param(&self, name: &str) -> Option<String> {
        reqwest::Url::parse(&format!("http://x/?{}", self.query()))
            .ok()?
            .query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(status: u16, v: Value) -> Reply {
        Reply {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: v.to_string().into_bytes(),
        }
    }

    pub fn empty(status: u16) -> Reply {
        Reply {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Reply {
        Reply {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
            body,
        }
    }

    pub fn redirect(location: &str) -> Reply {
        Reply {
            status: 302,
            headers: vec![("Location".into(), location.into())],
            body: Vec::new(),
        }
    }

    /// Graph's error envelope.
    pub fn error(status: u16, code: &str, message: &str) -> Reply {
        Reply::json(
            status,
            serde_json::json!({ "error": { "code": code, "message": message } }),
        )
    }
}

pub struct Mock {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<Req>>>,
    stop: Arc<AtomicBool>,
}

impl Mock {
    pub fn start(handler: impl Fn(&Req) -> Reply + Send + Sync + 'static) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("mock address");
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<dyn Fn(&Req) -> Reply + Send + Sync> = Arc::new(handler);
        let (log2, stop2) = (log.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop2.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (log, handler) = (log2.clone(), handler.clone());
                std::thread::spawn(move || serve(stream, &log, handler.as_ref()));
            }
        });
        Mock { addr, log, stop }
    }

    /// `http://127.0.0.1:<port>`
    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// What `Graph` uses as its base in tests.
    pub fn graph_base(&self) -> String {
        format!("{}/v1.0", self.origin())
    }

    pub fn requests(&self) -> Vec<Req> {
        self.log.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    /// Requests whose path ends with `suffix`.
    pub fn hits(&self, suffix: &str) -> Vec<Req> {
        self.requests()
            .into_iter()
            .filter(|r| r.path().ends_with(suffix))
            .collect()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop so its thread ends.
        let _ = TcpStream::connect(self.addr);
    }
}

fn find(buf: &[u8], needle: &[u8]) -> Option<usize> {
    buf.windows(needle.len()).position(|w| w == needle)
}

fn serve(
    mut stream: TcpStream,
    log: &Mutex<Vec<Req>>,
    handler: &(dyn Fn(&Req) -> Reply + Send + Sync),
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(i) = find(&buf, &HEADER_END) {
            break i;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (
        first.next().unwrap_or("").to_string(),
        first.next().unwrap_or("").to_string(),
    );
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let want: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + HEADER_END.len()..].to_vec();
    while body.len() < want {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    let req = Req {
        method,
        target,
        headers,
        body,
    };
    log.lock().unwrap().push(req.clone());
    let reply = handler(&req);
    let mut out = Vec::new();
    out.extend_from_slice(format!("HTTP/1.1 {} Mock", reply.status).as_bytes());
    out.extend_from_slice(&CRLF);
    for (k, v) in &reply.headers {
        out.extend_from_slice(format!("{k}: {v}").as_bytes());
        out.extend_from_slice(&CRLF);
    }
    out.extend_from_slice(format!("Content-Length: {}", reply.body.len()).as_bytes());
    out.extend_from_slice(&CRLF);
    out.extend_from_slice(b"Connection: close");
    out.extend_from_slice(&CRLF);
    out.extend_from_slice(&CRLF);
    out.extend_from_slice(&reply.body);
    let _ = stream.write_all(&out);
    let _ = stream.flush();
}
