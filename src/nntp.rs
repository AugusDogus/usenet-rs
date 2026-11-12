//! NNTP over TLS: fetching article bodies and checking that articles exist, across
//! servers tried in priority order.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::timeout;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub host: String,
    pub port: u16,
    #[serde(default = "yes")]
    pub tls: bool,
    pub username: String,
    pub password: String,
    /// The most connections torrential opens to this server.
    #[serde(default = "eight")]
    pub connections: u16,
    /// Lower is tried first, as in SABnzbd. Backup servers get a higher number.
    #[serde(default)]
    pub priority: i32,
}

fn yes() -> bool {
    true
}

fn eight() -> u16 {
    8
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Server({}:{}, priority {})", self.host, self.port, self.priority)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NntpError {
    /// No configured server has the article; it expired or was taken down.
    Missing { message_id: String },
    Connect { server: String, reason: String },
    Login { server: String, reply: String },
    Reply { server: String, command: &'static str, reply: String },
    Io { server: String, reason: String },
    /// Every server failed to answer for a reason other than a missing article.
    Unreachable { message_id: String, servers: String },
}

impl fmt::Display for NntpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { message_id } => write!(f, "article <{message_id}> is missing on every server"),
            Self::Connect { server, reason } => write!(f, "could not connect to {server}: {reason}"),
            Self::Login { server, reply } => write!(f, "{server} rejected the login: {reply}"),
            Self::Reply { server, command, reply } => write!(f, "{server} answered {command} with: {reply}"),
            Self::Io { server, reason } => write!(f, "lost the connection to {server}: {reason}"),
            Self::Unreachable { message_id, servers } => {
                write!(f, "no server could send article <{message_id}> ({servers})")
            }
        }
    }
}

impl std::error::Error for NntpError {}

const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a server that refused a connection or login is left alone.
const BACKOFF: Duration = Duration::from_secs(60);

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub struct Connection {
    io: BufReader<Box<dyn Io>>,
    label: String,
    broken: bool,
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            use rustls_platform_verifier::ConfigVerifierExt;
            Arc::new(rustls::ClientConfig::with_platform_verifier().expect("the platform certificate verifier loads"))
        })
        .clone()
}

impl Connection {
    pub async fn open(server: &Server) -> Result<Self, NntpError> {
        let label = format!("{}:{}", server.host, server.port);
        let connect_error = |reason: String| NntpError::Connect { server: label.clone(), reason };
        let tcp = timeout(IO_TIMEOUT, TcpStream::connect((server.host.as_str(), server.port)))
            .await
            .map_err(|_| connect_error("timed out".into()))?
            .map_err(|e| connect_error(e.to_string()))?;
        let io: Box<dyn Io> = if server.tls {
            let name = rustls::pki_types::ServerName::try_from(server.host.clone())
                .map_err(|e| connect_error(format!("bad host name: {e}")))?;
            let tls = tokio_rustls::TlsConnector::from(tls_config());
            let stream = timeout(IO_TIMEOUT, tls.connect(name, tcp))
                .await
                .map_err(|_| connect_error("TLS handshake timed out".into()))?
                .map_err(|e| connect_error(format!("TLS: {e}")))?;
            Box::new(stream)
        } else {
            Box::new(tcp)
        };
        let mut conn = Self { io: BufReader::with_capacity(256 * 1024, io), label: label.clone(), broken: false };
        let greeting = conn.reply().await?;
        if !greeting.starts_with("200") && !greeting.starts_with("201") {
            return Err(connect_error(format!("unexpected greeting: {greeting}")));
        }
        let user = conn.command(&format!("AUTHINFO USER {}", server.username), "AUTHINFO").await?;
        let login = if user.starts_with("381") {
            conn.command(&format!("AUTHINFO PASS {}", server.password), "AUTHINFO").await?
        } else {
            user
        };
        if !login.starts_with("281") {
            return Err(NntpError::Login { server: conn.label.clone(), reply: login });
        }
        Ok(conn)
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// The decoded-ready body: dot-stuffing removed, lines kept with their CRLF.
    pub async fn body(&mut self, message_id: &str) -> Result<Vec<u8>, NntpError> {
        let reply = self.command(&format!("BODY <{message_id}>"), "BODY").await?;
        match &reply[..3.min(reply.len())] {
            "222" => self.multiline().await,
            "430" | "423" => Err(NntpError::Missing { message_id: message_id.to_string() }),
            _ => Err(self.unexpected("BODY", reply)),
        }
    }

    pub async fn stat(&mut self, message_id: &str) -> Result<bool, NntpError> {
        let reply = self.command(&format!("STAT <{message_id}>"), "STAT").await?;
        match &reply[..3.min(reply.len())] {
            "223" => Ok(true),
            "430" | "423" => Ok(false),
            _ => Err(self.unexpected("STAT", reply)),
        }
    }

    fn unexpected(&mut self, command: &'static str, reply: String) -> NntpError {
        self.broken = true;
        NntpError::Reply { server: self.label.clone(), command, reply }
    }

    fn io_error(&mut self, reason: String) -> NntpError {
        self.broken = true;
        NntpError::Io { server: self.label.clone(), reason }
    }

    async fn command(&mut self, line: &str, _name: &'static str) -> Result<String, NntpError> {
        let sent = timeout(IO_TIMEOUT, self.io.get_mut().write_all(format!("{line}\r\n").as_bytes())).await;
        match sent {
            Ok(Ok(())) => self.reply().await,
            Ok(Err(e)) => Err(self.io_error(e.to_string())),
            Err(_) => Err(self.io_error("timed out sending a command".into())),
        }
    }

    async fn reply(&mut self) -> Result<String, NntpError> {
        let mut line = Vec::new();
        match timeout(IO_TIMEOUT, self.io.read_until(b'\n', &mut line)).await {
            Ok(Ok(0)) => Err(self.io_error("the server closed the connection".into())),
            Ok(Ok(_)) => Ok(String::from_utf8_lossy(&line).trim_end().to_string()),
            Ok(Err(e)) => Err(self.io_error(e.to_string())),
            Err(_) => Err(self.io_error(format!("no reply for {}s", IO_TIMEOUT.as_secs()))),
        }
    }

    async fn multiline(&mut self) -> Result<Vec<u8>, NntpError> {
        let mut body = Vec::with_capacity(800 * 1024);
        let mut line = Vec::with_capacity(256);
        loop {
            line.clear();
            match timeout(IO_TIMEOUT, self.io.read_until(b'\n', &mut line)).await {
                Ok(Ok(0)) => return Err(self.io_error("the server closed the connection mid-article".into())),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(self.io_error(e.to_string())),
                Err(_) => return Err(self.io_error("the article stalled".into())),
            }
            if line == b".\r\n" || line == b".\n" {
                return Ok(body);
            }
            body.extend_from_slice(line.strip_prefix(b".").unwrap_or(&line));
        }
    }
}

struct Slot {
    server: Server,
    state: Mutex<SlotState>,
}

#[derive(Default)]
struct SlotState {
    idle: Vec<Connection>,
    open: u16,
    backoff_until: Option<Instant>,
    last_error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PoolStats {
    pub bytes: u64,
    pub articles: u64,
    /// Connections open now, busy or idle.
    pub connections: u32,
}

/// Connections across servers. A request uses the first server, by priority, with a free
/// connection, and moves to the next server when the article is missing there.
#[derive(Clone)]
pub struct Pool(Arc<PoolInner>);

struct PoolInner {
    slots: Vec<Slot>,
    freed: Notify,
    bytes: AtomicU64,
    articles: AtomicU64,
}

enum Op<'a> {
    Body(&'a str),
    Stat(&'a str),
}

enum Answer {
    Body(Vec<u8>),
    Exists,
}

impl Pool {
    pub fn new(mut servers: Vec<Server>) -> Self {
        servers.sort_by_key(|s| s.priority);
        let slots = servers.into_iter().map(|server| Slot { server, state: Mutex::default() }).collect();
        Self(Arc::new(PoolInner { slots, freed: Notify::new(), bytes: AtomicU64::new(0), articles: AtomicU64::new(0) }))
    }

    pub fn stats(&self) -> PoolStats {
        let connections = self.0.slots.iter().map(|s| u32::from(s.state.lock().unwrap().open)).sum();
        PoolStats {
            bytes: self.0.bytes.load(Ordering::Relaxed),
            articles: self.0.articles.load(Ordering::Relaxed),
            connections,
        }
    }

    /// The most connections the pool would open.
    pub fn capacity(&self) -> usize {
        self.0.slots.iter().map(|s| usize::from(s.server.connections)).sum()
    }

    pub async fn body(&self, message_id: &str) -> Result<Vec<u8>, NntpError> {
        match self.run(message_id, Op::Body(message_id)).await? {
            Answer::Body(body) => {
                self.0.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                self.0.articles.fetch_add(1, Ordering::Relaxed);
                Ok(body)
            }
            Answer::Exists => unreachable!("BODY answers with a body"),
        }
    }

    /// True when at least one server has the article.
    pub async fn stat(&self, message_id: &str) -> Result<bool, NntpError> {
        match self.run(message_id, Op::Stat(message_id)).await {
            Ok(_) => Ok(true),
            Err(NntpError::Missing { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn run(&self, message_id: &str, op: Op<'_>) -> Result<Answer, NntpError> {
        let mut tried = vec![false; self.0.slots.len()];
        let mut missing = false;
        let mut failures = 0;
        while let Some((slot, mut conn)) = self.acquire(&tried).await {
            let result = match op {
                Op::Body(id) => conn.body(id).await.map(Answer::Body),
                Op::Stat(id) => match conn.stat(id).await {
                    Ok(true) => Ok(Answer::Exists),
                    Ok(false) => Err(NntpError::Missing { message_id: id.to_string() }),
                    Err(e) => Err(e),
                },
            };
            let error = match result {
                Ok(answer) => {
                    self.release(slot, conn);
                    return Ok(answer);
                }
                Err(e) => e,
            };
            self.release(slot, conn);
            if matches!(error, NntpError::Missing { .. }) {
                missing = true;
                tried[slot] = true;
                continue;
            }
            self.0.slots[slot].state.lock().unwrap().last_error = Some(error.to_string());
            // A dropped connection is retried once on the same server before moving on.
            failures += 1;
            if failures >= 2 {
                tried[slot] = true;
                failures = 0;
            }
        }
        let errors: Vec<String> = self
            .0
            .slots
            .iter()
            .filter_map(|s| Some(format!("{}: {}", s.server.host, s.state.lock().unwrap().last_error.clone()?)))
            .collect();
        if missing && errors.is_empty() {
            Err(NntpError::Missing { message_id: message_id.to_string() })
        } else {
            let servers = if errors.is_empty() { "no server is configured".into() } else { errors.join("; ") };
            Err(NntpError::Unreachable { message_id: message_id.to_string(), servers })
        }
    }

    /// A connection to the first usable server not yet tried. None once every server was tried.
    async fn acquire(&self, tried: &[bool]) -> Option<(usize, Connection)> {
        loop {
            let freed = self.0.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            let now = Instant::now();
            let mut usable = false;
            let mut to_open = None;
            for (i, slot) in self.0.slots.iter().enumerate() {
                let mut state = slot.state.lock().unwrap();
                if tried[i] || state.backoff_until.is_some_and(|t| t > now) {
                    continue;
                }
                usable = true;
                if let Some(conn) = state.idle.pop() {
                    return Some((i, conn));
                }
                if to_open.is_none() && state.open < slot.server.connections {
                    state.open += 1;
                    to_open = Some(i);
                }
            }
            if !usable {
                return None;
            }
            let Some(i) = to_open else {
                freed.await;
                continue;
            };
            let slot = &self.0.slots[i];
            match Connection::open(&slot.server).await {
                Ok(conn) => return Some((i, conn)),
                Err(e) => {
                    let mut state = slot.state.lock().unwrap();
                    state.open -= 1;
                    state.backoff_until = Some(Instant::now() + BACKOFF);
                    state.last_error = Some(e.to_string());
                    drop(state);
                    self.0.freed.notify_waiters();
                }
            }
        }
    }

    fn release(&self, slot: usize, conn: Connection) {
        let mut state = self.0.slots[slot].state.lock().unwrap();
        if conn.broken {
            state.open -= 1;
        } else {
            state.last_error = None;
            state.idle.push(conn);
        }
        drop(state);
        self.0.freed.notify_one();
    }
}
