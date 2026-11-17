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
    /// The most connections newsreel opens to this server.
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
    Missing {
        message_id: String,
    },
    Connect {
        server: String,
        reason: String,
    },
    Login {
        server: String,
        reply: String,
    },
    Reply {
        server: String,
        command: &'static str,
        reply: String,
    },
    Io {
        server: String,
        reason: String,
    },
    /// Every server failed to answer for a reason other than a missing article.
    Unreachable {
        message_id: String,
        servers: String,
    },
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

/// Silence this long mid-command means the connection is dead.
const IO_TIMEOUT: Duration = Duration::from_secs(20);
/// Connecting, TLS, greeting, and login. A server at its connection limit can accept the
/// socket and then say nothing, so this is short; the pool moves on to another server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
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
        timeout(CONNECT_TIMEOUT, Self::connect(server, &label)).await.map_err(|_| NntpError::Connect {
            server: label.clone(),
            reason: format!(
                "no login within {}s; the server may be at its connection limit",
                CONNECT_TIMEOUT.as_secs()
            ),
        })?
    }

    async fn connect(server: &Server, label: &str) -> Result<Self, NntpError> {
        let label = label.to_string();
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
        match status(&reply) {
            b"222" => self.multiline().await,
            b"430" | b"423" => Err(NntpError::Missing { message_id: message_id.to_string() }),
            _ => Err(self.unexpected("BODY", reply)),
        }
    }

    pub async fn stat(&mut self, message_id: &str) -> Result<bool, NntpError> {
        let reply = self.command(&format!("STAT <{message_id}>"), "STAT").await?;
        match status(&reply) {
            b"223" => Ok(true),
            b"430" | b"423" => Ok(false),
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

/// The three-digit code a reply starts with, or what there is of it. Byte-wise, since a
/// misbehaving server can send any bytes.
fn status(reply: &str) -> &[u8] {
    let bytes = reply.as_bytes();
    &bytes[..bytes.len().min(3)]
}

struct Slot {
    server: Server,
    state: Mutex<SlotState>,
    counters: Counters,
}

#[derive(Default)]
struct Counters {
    answered: AtomicU64,
    missing: AtomicU64,
    failed: AtomicU64,
    /// Connections that could not be opened.
    refused: AtomicU64,
    /// Answers slower than [`SLOW`].
    slow: AtomicU64,
    millis: AtomicU64,
}

/// How one server has served this pool, for diagnosing slow or failing servers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ServerReport {
    pub host: String,
    pub answered: u64,
    pub missing: u64,
    pub failed: u64,
    pub refused: u64,
    pub slow: u64,
    /// Mean time per answered or missing request.
    pub mean_millis: u64,
    pub last_error: Option<String>,
}

const SLOW: Duration = Duration::from_secs(5);

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
        let slots = servers
            .into_iter()
            .map(|server| Slot { server, state: Mutex::default(), counters: Counters::default() })
            .collect();
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

    pub fn report(&self) -> Vec<ServerReport> {
        self.0
            .slots
            .iter()
            .map(|s| {
                let c = &s.counters;
                let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
                let timed = load(&c.answered) + load(&c.missing);
                ServerReport {
                    host: s.server.host.clone(),
                    answered: load(&c.answered),
                    missing: load(&c.missing),
                    failed: load(&c.failed),
                    refused: load(&c.refused),
                    slow: load(&c.slow),
                    mean_millis: load(&c.millis).checked_div(timed).unwrap_or(0),
                    last_error: s.state.lock().unwrap().last_error.clone(),
                }
            })
            .collect()
    }

    /// The most connections the pool uses at once: those of its largest priority tier, since
    /// a lower tier is only asked for what the tiers above it lack.
    pub fn capacity(&self) -> usize {
        let mut tiers: Vec<(i32, usize)> = Vec::new();
        for s in &self.0.slots {
            match tiers.last_mut() {
                Some((priority, total)) if *priority == s.server.priority => {
                    *total += usize::from(s.server.connections)
                }
                _ => tiers.push((s.server.priority, usize::from(s.server.connections))),
            }
        }
        tiers.into_iter().map(|(_, total)| total).max().unwrap_or(0)
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
            let asked = Instant::now();
            let result = match op {
                Op::Body(id) => conn.body(id).await.map(Answer::Body),
                Op::Stat(id) => match conn.stat(id).await {
                    Ok(true) => Ok(Answer::Exists),
                    Ok(false) => Err(NntpError::Missing { message_id: id.to_string() }),
                    Err(e) => Err(e),
                },
            };
            let counters = &self.0.slots[slot].counters;
            let took = asked.elapsed();
            if took >= SLOW {
                counters.slow.fetch_add(1, Ordering::Relaxed);
            }
            match &result {
                Ok(_) | Err(NntpError::Missing { .. }) => {
                    let count = if result.is_ok() { &counters.answered } else { &counters.missing };
                    count.fetch_add(1, Ordering::Relaxed);
                    counters.millis.fetch_add(took.as_millis() as u64, Ordering::Relaxed);
                }
                Err(_) => {
                    counters.failed.fetch_add(1, Ordering::Relaxed);
                }
            }
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

    /// A connection to a usable server not yet tried, from the best priority tier that has
    /// one: a busy tier is waited for rather than skipped, so backup servers only see articles
    /// the servers above them lack, or requests while those servers are refusing connections.
    /// None once every server was tried.
    async fn acquire(&self, tried: &[bool]) -> Option<(usize, Connection)> {
        loop {
            let freed = self.0.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            let now = Instant::now();
            let mut tier = None;
            let mut to_open = None;
            for (i, slot) in self.0.slots.iter().enumerate() {
                let mut state = slot.state.lock().unwrap();
                // A server that refused a new connection still serves on those it has open.
                let backing_off = state.backoff_until.is_some_and(|t| t > now);
                if tried[i] || (backing_off && state.open == 0) {
                    continue;
                }
                // Slots are sorted by priority, so the first usable one names the tier.
                if *tier.get_or_insert(slot.server.priority) != slot.server.priority {
                    break;
                }
                if let Some(conn) = state.idle.pop() {
                    return Some((i, conn));
                }
                if !backing_off && to_open.is_none() && state.open < slot.server.connections {
                    state.open += 1;
                    to_open = Some(i);
                }
            }
            // No tier means every server was tried or is refusing connections.
            tier?;
            let Some(i) = to_open else {
                freed.await;
                continue;
            };
            let slot = &self.0.slots[i];
            match Connection::open(&slot.server).await {
                Ok(conn) => return Some((i, conn)),
                Err(e) => {
                    slot.counters.refused.fetch_add(1, Ordering::Relaxed);
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
            // A refusal stays reported until its backoff ends, even while open connections work.
            if state.backoff_until.is_none_or(|t| t <= Instant::now()) {
                state.last_error = None;
            }
            state.idle.push(conn);
        }
        drop(state);
        self.0.freed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeServer, Posting};

    fn posting() -> Posting {
        let data: Vec<u8> = (0..40_000u32).map(|i| (i % 253) as u8).collect();
        Posting::new("tiers", &[("a.bin", &data)], 1000)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_busy_primary_is_waited_for_and_a_backup_only_serves_what_it_lacks() {
        let (primary, backup) = (FakeServer::start(), FakeServer::start());
        let posting = posting();
        primary.add(&posting);
        backup.add(&posting);
        let pool = Pool::new(vec![backup.config(4, 1), primary.config(2, 0)]);
        assert_eq!(pool.capacity(), 4);
        let ids: Vec<&str> = posting.ids().collect();
        let bodies = futures_util::future::join_all(ids.iter().map(|id| pool.body(id))).await;
        assert!(bodies.iter().all(Result::is_ok));
        assert_eq!((primary.bodies(), backup.bodies()), (ids.len() as u64, 0));

        primary.remove(ids[0]);
        pool.body(ids[0]).await.unwrap();
        assert_eq!((backup.served(ids[0]), backup.bodies()), (1, 1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusing_primary_hands_its_requests_to_the_backup() {
        let (primary, backup) = (FakeServer::start(), FakeServer::start());
        let posting = posting();
        primary.add(&posting);
        backup.add(&posting);
        primary.set_down(true);
        let pool = Pool::new(vec![primary.config(2, 0), backup.config(2, 1)]);
        let id = posting.ids().next().unwrap();
        pool.body(id).await.unwrap();
        assert_eq!(backup.served(id), 1);
        assert_eq!(pool.report()[0].refused, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_server_at_its_connection_limit_keeps_serving_on_the_connections_it_allows() {
        let server = FakeServer::start();
        let posting = posting();
        server.add(&posting);
        server.set_max_sessions(1);
        let pool = Pool::new(vec![server.config(3, 0)]);
        let ids: Vec<&str> = posting.ids().collect();
        let bodies = futures_util::future::join_all(ids.iter().map(|id| pool.body(id))).await;
        assert!(bodies.iter().all(Result::is_ok), "{bodies:?}");
        let report = &pool.report()[0];
        assert!(report.refused >= 1);
        assert!(report.last_error.as_deref().is_some_and(|e| e.contains("too many connections")), "{report:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reply_that_is_not_ascii_is_an_error_not_a_panic() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            write.write_all(b"200 hi\r\n").await.unwrap();
            for reply in ["381 more\r\n", "281 ok\r\n", "22\u{20ac}x\r\n", "2\u{20ac}\r\n"] {
                lines.next_line().await.unwrap();
                write.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let server = Server {
            host: "127.0.0.1".into(),
            port,
            tls: false,
            username: "u".into(),
            password: "p".into(),
            connections: 1,
            priority: 0,
        };
        let mut conn = Connection::open(&server).await.unwrap();
        assert!(matches!(conn.body("a@x").await, Err(NntpError::Reply { .. })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_article_on_no_server_is_missing() {
        let server = FakeServer::start();
        let pool = Pool::new(vec![server.config(1, 0)]);
        assert!(matches!(pool.body("nope@x").await, Err(NntpError::Missing { .. })));
        assert!(!pool.stat("nope@x").await.unwrap());
    }
}
