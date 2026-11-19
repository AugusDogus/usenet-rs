//! NNTP over TLS: fetching article bodies and checking that articles exist, across
//! servers tried in priority order.
//!
//! A [`Pool`] keeps logged-in connections to each [`Server`] and pipelines requests on them:
//! each connection carries up to [`Server::pipeline`] commands at once, and their replies
//! come back in the order the commands went out.
//!
//! # Priority
//!
//! A request is [`Urgent`](Priority::Urgent), for a seek or the first reads of a release
//! about to play, or [`Background`](Priority::Background), for a download; a pool handle made
//! by [`Pool::with_priority`] sends its requests at its priority. Requests in flight share
//! the link, so an urgent one is fast only when few others stream beside it:
//!
//! - Background requests are held to a limit the pool tunes as it runs: the fewest in flight
//!   that keep throughput within a few percent of the best it has seen (see
//!   [`PoolStats::background_limit`]). On a link that a few connections fill, that leaves
//!   most connections idle and most of the bandwidth for urgent requests, without slowing the
//!   download.
//! - Urgent requests are not limited, take a freed slot before any waiting background request,
//!   and, like every request, go to an idle connection when one is open.
//!
//! # Head-of-line blocking
//!
//! A reply queued behind a slow one on the same connection waits for it. So a request goes
//! to an idle connection when one is open, and a new connection is opened in the background
//! when none is idle and the server allows more; only then does it queue behind others, on
//! the connection with the fewest in flight. A pipeline of 1 never queues.

mod connection;
mod error;
mod limit;
mod reply;

use std::fmt;
use std::num::NonZeroU8;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc, oneshot};

use connection::{Job, Outcome, Owner, Session};
pub use error::{ConnectFailure, IoFailure, NntpError};
use limit::Limit;
pub use reply::Violation;
#[cfg(fuzzing)]
#[doc(hidden)]
pub use reply::fuzzing;

/// A news server and an account on it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    /// The host name or address.
    pub host: String,
    /// Usually 563 for TLS and 119 without.
    pub port: u16,
    /// Whether to connect over TLS. True unless set.
    #[serde(default = "yes")]
    pub tls: bool,
    /// The account's username.
    pub username: String,
    /// The account's password.
    pub password: String,
    /// The most connections the pool opens to this server. 8 unless set.
    #[serde(default = "eight")]
    pub connections: u16,
    /// Lower is tried first, as in SABnzbd. Backup servers get a higher number.
    #[serde(default)]
    pub priority: i32,
    /// The most commands in flight on one connection; 1 sends each command only after the
    /// last reply. [`Server::DEFAULT_PIPELINE`] unless set.
    #[serde(default = "default_pipeline")]
    pub pipeline: NonZeroU8,
}

impl Server {
    /// Enough commands in flight to keep a connection busy across a round trip to a distant
    /// server, without queueing a seek behind many articles.
    pub const DEFAULT_PIPELINE: NonZeroU8 = NonZeroU8::MIN.saturating_add(3);
}

fn yes() -> bool {
    true
}

fn eight() -> u16 {
    8
}

fn default_pipeline() -> NonZeroU8 {
    Server::DEFAULT_PIPELINE
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Server({}:{}, priority {})", self.host, self.port, self.priority)
    }
}

/// How long a server that refused a connection or login is left alone.
const BACKOFF: Duration = Duration::from_secs(60);
/// Answers slower than this count as slow in [`ServerReport::slow`].
const SLOW: Duration = Duration::from_secs(5);
/// The longest message-ID RFC 5536 allows, angle brackets included.
const MAX_MESSAGE_ID: usize = 250;

/// True when `message_id` can go into a command as `<message_id>`: printable ASCII, no
/// spaces or angle brackets, so it can neither end the command early nor add one.
fn sendable(message_id: &str) -> bool {
    !message_id.is_empty()
        && message_id.len() + 2 <= MAX_MESSAGE_ID
        && message_id.bytes().all(|b| b.is_ascii_graphic() && b != b'<' && b != b'>')
}

struct Slot {
    server: Server,
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
    /// The server's host.
    pub host: String,
    /// Requests the server answered with the article or its existence.
    pub answered: u64,
    /// Requests for articles the server lacks.
    pub missing: u64,
    /// Requests that failed on the server: a dropped connection or an unexpected reply.
    pub failed: u64,
    /// Connections that could not be opened, or whose login failed.
    pub refused: u64,
    /// Answers that took longer than 5 seconds.
    pub slow: u64,
    /// Mean time per answered or missing request, waiting behind pipelined ones included.
    pub mean_millis: u64,
    /// Why the server last failed, until it next answers.
    pub last_error: Option<NntpError>,
}

/// One open connection, as the pool sees it.
struct Conn {
    id: u64,
    jobs: mpsc::UnboundedSender<Job>,
    /// Commands sent to it whose replies have not been read.
    in_flight: usize,
}

#[derive(Default)]
struct SlotState {
    conns: Vec<Conn>,
    /// Connections being opened.
    opening: u16,
    backoff_until: Option<Instant>,
    last_error: Option<NntpError>,
}

/// What the pool has done so far.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PoolStats {
    /// Body bytes received.
    pub bytes: u64,
    /// Bodies received.
    pub articles: u64,
    /// Connections open now, busy or idle.
    pub connections: u32,
    /// The most background requests the pool lets be in flight now; see
    /// [Priority](self#priority).
    pub background_limit: u32,
}

/// How soon a request is needed. See [Priority](self#priority).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Priority {
    /// Needed now: a seek, or the first reads of a release about to play.
    Urgent,
    /// Wanted soon: a download, or reading ahead. What [`Pool::new`] makes.
    #[default]
    Background,
}

/// Connections across servers. A request uses the best server, by priority, that has room
/// for it, and moves to the next server when the article is missing there. Cloning shares
/// the connections. Dropping the last clone closes them, with a QUIT.
///
/// Must be used within a Tokio runtime: each connection runs as a task.
///
/// ```no_run
/// use usenet::{Pool, Server};
///
/// # async fn run() -> Result<(), usenet::nntp::NntpError> {
/// let pool = Pool::new(vec![Server {
///     host: "news.example.com".into(),
///     port: 563,
///     tls: true,
///     username: "user".into(),
///     password: "secret".into(),
///     connections: 8,
///     priority: 0,
///     pipeline: Server::DEFAULT_PIPELINE,
/// }]);
/// if pool.stat("part1of10.abc@example.com").await? {
///     let body = pool.body("part1of10.abc@example.com").await?;
///     println!("{} bytes of yEnc", body.len());
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Pool {
    inner: Arc<PoolInner>,
    priority: Priority,
}

struct PoolInner {
    slots: Vec<Slot>,
    /// Each slot's connections, under one lock so a request picks among them atomically.
    states: Mutex<Vec<SlotState>>,
    /// Signalled whenever a connection gets room, opens, or fails to open, and whenever a
    /// background request may go where it could not before.
    freed: Notify,
    /// Tunes how many background requests may be in flight.
    limit: Mutex<Limit>,
    /// The limit as last tuned.
    allowed: AtomicUsize,
    /// Background requests that hold a slot on a connection.
    background: AtomicUsize,
    /// Urgent requests waiting for a slot.
    urgent_waiting: AtomicUsize,
    bytes: AtomicU64,
    articles: AtomicU64,
    /// Bytes read from every connection, replies and all, for measuring the link.
    received: AtomicU64,
    next_id: AtomicU64,
}

/// Where a request was sent.
struct Sent<'a> {
    slot: usize,
    conn: u64,
    jobs: mpsc::UnboundedSender<Job>,
    /// Held by a background request until it ends.
    _background: Option<Background<'a>>,
}

/// A background request's place under the limit, given back when dropped, even when the
/// caller gives up on the request.
struct Background<'a>(&'a PoolInner);

impl Drop for Background<'_> {
    fn drop(&mut self) {
        self.0.background.fetch_sub(1, Ordering::SeqCst);
        self.0.freed.notify_waiters();
    }
}

/// Counts an urgent request as waiting for a slot while held.
struct Counted<'a>(&'a AtomicUsize, &'a Notify);

impl<'a> Counted<'a> {
    fn new(count: &'a AtomicUsize, notify: &'a Notify) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count, notify)
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        // The last urgent request to leave lets background requests go again.
        if self.0.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.1.notify_waiters();
        }
    }
}

enum Pick<'a> {
    Sent(Sent<'a>),
    /// Every usable connection is full; one may free up or open.
    Wait,
    /// Every server was tried or is refusing connections.
    Exhausted,
}

/// A connection's place in the pool, held by its task.
struct Seat {
    pool: Weak<PoolInner>,
    slot: usize,
    id: u64,
}

impl Owner for Seat {
    fn answered(&self) {
        let Some(pool) = self.pool.upgrade() else { return };
        if let Some(conn) = pool.states.lock()[self.slot].conns.iter_mut().find(|c| c.id == self.id) {
            conn.in_flight = conn.in_flight.saturating_sub(1);
        }
        pool.freed.notify_waiters();
    }

    fn received(&self, bytes: usize) {
        if let Some(pool) = self.pool.upgrade() {
            pool.received.fetch_add(bytes as u64, Ordering::Relaxed);
        }
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            pool.forget(self.slot, self.id);
        }
    }
}

/// The most requests the slots serve at once, connections times pipeline depth, of the
/// largest priority tier.
fn capacity(slots: &[Slot]) -> usize {
    let mut tiers: Vec<(i32, usize)> = Vec::new();
    for s in slots {
        let room = usize::from(s.server.connections) * usize::from(s.server.pipeline.get());
        match tiers.last_mut() {
            Some((priority, total)) if *priority == s.server.priority => *total += room,
            _ => tiers.push((s.server.priority, room)),
        }
    }
    tiers.into_iter().map(|(_, total)| total).max().unwrap_or(0)
}

impl PoolInner {
    fn forget(&self, slot: usize, id: u64) {
        self.states.lock()[slot].conns.retain(|c| c.id != id);
        self.freed.notify_waiters();
    }
}

impl Pool {
    /// A pool over `servers`, which connects only when asked for an article.
    pub fn new(mut servers: Vec<Server>) -> Self {
        servers.sort_by_key(|s| s.priority);
        let states = servers.iter().map(|_| SlotState::default()).collect();
        let slots: Vec<Slot> =
            servers.into_iter().map(|server| Slot { server, counters: Counters::default() }).collect();
        let limit = Limit::new(capacity(&slots), Instant::now());
        let allowed = AtomicUsize::new(limit.get());
        let inner = Arc::new(PoolInner {
            slots,
            states: Mutex::new(states),
            freed: Notify::new(),
            limit: Mutex::new(limit),
            allowed,
            background: AtomicUsize::new(0),
            urgent_waiting: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
            articles: AtomicU64::new(0),
            received: AtomicU64::new(0),
            next_id: AtomicU64::new(0),
        });
        Self { inner, priority: Priority::Background }
    }

    /// A handle on the same connections whose requests go at `priority`.
    pub fn with_priority(&self, priority: Priority) -> Self {
        Self { inner: self.inner.clone(), priority }
    }

    /// The priority this handle's requests go at.
    pub fn priority(&self) -> Priority {
        self.priority
    }

    /// What the pool has fetched, and how many connections it holds.
    pub fn stats(&self) -> PoolStats {
        let connections = self.inner.states.lock().iter().map(|s| s.conns.len() as u32).sum();
        PoolStats {
            bytes: self.inner.bytes.load(Ordering::Relaxed),
            articles: self.inner.articles.load(Ordering::Relaxed),
            connections,
            background_limit: self.inner.allowed.load(Ordering::Relaxed) as u32,
        }
    }

    /// How each server has done, in priority order.
    pub fn report(&self) -> Vec<ServerReport> {
        let states = self.inner.states.lock();
        self.inner
            .slots
            .iter()
            .zip(states.iter())
            .map(|(s, state)| {
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
                    last_error: state.last_error.clone(),
                }
            })
            .collect()
    }

    /// The most requests the pool serves at once, connections times pipeline depth, of its
    /// largest priority tier, since a lower tier is only asked for what the tiers above it
    /// lack. Running this many requests at a time keeps every connection busy.
    pub fn capacity(&self) -> usize {
        capacity(&self.inner.slots)
    }

    /// The article's body: dot-stuffing removed, lines kept with their CRLF, ready for yEnc.
    ///
    /// Fails with [`NntpError::Missing`] when no server has the article, and with
    /// [`NntpError::Unreachable`] when some server could not be asked.
    pub async fn body(&self, message_id: &str) -> Result<Vec<u8>, NntpError> {
        let body = self.run(message_id, |message_id, reply| Job::Body { message_id, reply }).await?;
        self.inner.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
        self.inner.articles.fetch_add(1, Ordering::Relaxed);
        let changed = {
            let mut limit = self.inner.limit.lock();
            let changed = limit.tick(self.inner.received.load(Ordering::Relaxed), Instant::now());
            self.inner.allowed.store(limit.get(), Ordering::SeqCst);
            changed
        };
        if changed {
            self.inner.freed.notify_waiters();
        }
        Ok(body)
    }

    /// True when at least one server has the article.
    pub async fn stat(&self, message_id: &str) -> Result<bool, NntpError> {
        match self.run(message_id, |message_id, reply| Job::Stat { message_id, reply }).await {
            Ok(()) => Ok(true),
            Err(NntpError::Missing { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Asks servers in priority order until one has the article. A request whose reply breaks
    /// its connection is retried once on the same server, then moves on. One queued behind it
    /// is retried without counting against it: each break blames one request, so a server
    /// that keeps breaking connections runs out of requests to blame.
    async fn run<T>(
        &self,
        message_id: &str,
        job: impl Fn(String, oneshot::Sender<Outcome<T>>) -> Job,
    ) -> Result<T, NntpError> {
        if !sendable(message_id) {
            return Err(NntpError::InvalidMessageId { message_id: message_id.to_string() });
        }
        if self.inner.slots.is_empty() {
            return Err(NntpError::NoServers { message_id: message_id.to_string() });
        }
        let mut tried = vec![false; self.inner.slots.len()];
        let mut missing = false;
        let mut failures = 0;
        while let Some(sent) = self.acquire(&tried).await {
            let (reply, answer) = oneshot::channel();
            let asked = Instant::now();
            let outcome = match sent.jobs.send(job(message_id.to_string(), reply)) {
                Ok(()) => answer.await.unwrap_or(Outcome::Failed(NntpError::Io {
                    server: self.label(sent.slot),
                    failure: IoFailure::Closed,
                })),
                Err(_) => {
                    // The connection failed after it was picked.
                    self.inner.forget(sent.slot, sent.conn);
                    Outcome::Failed(NntpError::Io { server: self.label(sent.slot), failure: IoFailure::Closed })
                }
            };
            let counters = &self.inner.slots[sent.slot].counters;
            let took = asked.elapsed();
            if took >= SLOW {
                counters.slow.fetch_add(1, Ordering::Relaxed);
            }
            let error = match outcome {
                Outcome::Found(answer) => {
                    self.answered(sent.slot, &counters.answered, took);
                    return Ok(answer);
                }
                Outcome::Missing => {
                    self.answered(sent.slot, &counters.missing, took);
                    missing = true;
                    tried[sent.slot] = true;
                    continue;
                }
                Outcome::Failed(error) => error,
                // Another request broke the connection; this one was only queued behind it.
                Outcome::Dropped => continue,
            };
            counters.failed.fetch_add(1, Ordering::Relaxed);
            self.inner.states.lock()[sent.slot].last_error = Some(error);
            failures += 1;
            if failures >= 2 {
                tried[sent.slot] = true;
                failures = 0;
            }
        }
        let errors: Vec<NntpError> = self.inner.states.lock().iter().filter_map(|s| s.last_error.clone()).collect();
        if missing && errors.is_empty() {
            Err(NntpError::Missing { message_id: message_id.to_string() })
        } else {
            Err(NntpError::Unreachable { message_id: message_id.to_string(), errors })
        }
    }

    /// Counts an answer, and clears the server's last error unless it is backing off: a refusal
    /// stays reported until its backoff ends, even while open connections work.
    fn answered(&self, slot: usize, count: &AtomicU64, took: Duration) {
        count.fetch_add(1, Ordering::Relaxed);
        self.inner.slots[slot].counters.millis.fetch_add(took.as_millis() as u64, Ordering::Relaxed);
        let mut states = self.inner.states.lock();
        let state = &mut states[slot];
        if state.backoff_until.is_none_or(|t| t <= Instant::now()) {
            state.last_error = None;
        }
    }

    fn label(&self, slot: usize) -> String {
        let server = &self.inner.slots[slot].server;
        format!("{}:{}", server.host, server.port)
    }

    /// Room for one request on a usable server not yet tried, from the best priority tier
    /// that has one: a busy tier is waited for rather than skipped, so backup servers only see
    /// articles the servers above them lack, or requests while those servers are refusing
    /// connections. None once every server was tried.
    async fn acquire(&self, tried: &[bool]) -> Option<Sent<'_>> {
        let _waiting =
            (self.priority == Priority::Urgent).then(|| Counted::new(&self.inner.urgent_waiting, &self.inner.freed));
        loop {
            let freed = self.inner.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            match self.pick(tried) {
                Pick::Sent(sent) => return Some(sent),
                Pick::Exhausted => return None,
                Pick::Wait => freed.await,
            }
        }
    }

    fn pick(&self, tried: &[bool]) -> Pick<'_> {
        let now = Instant::now();
        let mut states = self.inner.states.lock();
        let background = self.priority == Priority::Background;
        if background {
            // Urgent requests take freed slots first.
            if self.inner.urgent_waiting.load(Ordering::SeqCst) > 0 {
                return Pick::Wait;
            }
            if self.inner.background.load(Ordering::SeqCst) >= self.inner.allowed.load(Ordering::SeqCst) {
                self.inner.limit.lock().bound();
                return Pick::Wait;
            }
        }
        let mut tier = None;
        // The connection with the fewest in flight that has room, and a server to open one on.
        let mut best: Option<(usize, usize, usize)> = None;
        let mut to_open = None;
        for (i, (slot, state)) in self.inner.slots.iter().zip(states.iter()).enumerate() {
            // A server that refused a new connection still serves on those it has open.
            let backing_off = state.backoff_until.is_some_and(|t| t > now);
            if tried[i] || (backing_off && state.conns.is_empty() && state.opening == 0) {
                continue;
            }
            // Slots are sorted by priority, so the first usable one names the tier.
            if *tier.get_or_insert(slot.server.priority) != slot.server.priority {
                break;
            }
            let depth = usize::from(slot.server.pipeline.get());
            for (k, conn) in state.conns.iter().enumerate() {
                if conn.in_flight < depth && best.is_none_or(|(_, _, load)| conn.in_flight < load) {
                    best = Some((i, k, conn.in_flight));
                }
            }
            let open = state.conns.len() + usize::from(state.opening);
            if to_open.is_none() && !backing_off && open < usize::from(slot.server.connections) {
                to_open = Some(i);
            }
        }
        // No tier means every server was tried or is refusing connections.
        if tier.is_none() {
            return Pick::Exhausted;
        }
        // Width before depth: a new connection beats queueing behind a busy one.
        if let Some(i) = to_open
            && best.is_none_or(|(_, _, load)| load > 0)
        {
            states[i].opening += 1;
            self.open(i);
        }
        let Some((i, k, _)) = best else { return Pick::Wait };
        let conn = &mut states[i].conns[k];
        conn.in_flight += 1;
        let _background = background.then(|| {
            self.inner.background.fetch_add(1, Ordering::SeqCst);
            Background(&self.inner)
        });
        Pick::Sent(Sent { slot: i, conn: conn.id, jobs: conn.jobs.clone(), _background })
    }

    /// Opens a connection to slot `i` in the background, then wakes waiting requests. The
    /// caller has counted it in `opening`.
    fn open(&self, i: usize) {
        let pool = self.inner.clone();
        tokio::spawn(async move {
            let slot = &pool.slots[i];
            let opened = Session::open(&slot.server).await;
            let mut states = pool.states.lock();
            let state = &mut states[i];
            state.opening = state.opening.saturating_sub(1);
            match opened {
                Ok(session) => {
                    let id = pool.next_id.fetch_add(1, Ordering::Relaxed);
                    let (jobs, queue) = mpsc::unbounded_channel();
                    state.conns.push(Conn { id, jobs, in_flight: 0 });
                    drop(states);
                    tokio::spawn(session.drive(queue, Seat { pool: Arc::downgrade(&pool), slot: i, id }));
                }
                Err(e) => {
                    slot.counters.refused.fetch_add(1, Ordering::Relaxed);
                    state.backoff_until = Some(Instant::now() + BACKOFF);
                    state.last_error = Some(e);
                    drop(states);
                }
            }
            pool.freed.notify_waiters();
        });
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
        assert_eq!(pool.capacity(), 4 * usize::from(Server::DEFAULT_PIPELINE.get()));
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
        assert!(matches!(pool.report()[0].last_error, Some(NntpError::Refused { .. })));
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
        assert!(
            report.last_error.as_ref().is_some_and(|e| e.to_string().contains("too many connections")),
            "{report:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_article_on_no_server_is_missing() {
        let server = FakeServer::start();
        let pool = Pool::new(vec![server.config(1, 0)]);
        assert!(matches!(pool.body("nope@x").await, Err(NntpError::Missing { .. })));
        assert!(!pool.stat("nope@x").await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_message_id_that_could_inject_a_command_is_refused_unsent() {
        let server = FakeServer::start();
        let pool = Pool::new(vec![server.config(1, 0)]);
        for id in ["", "a b@x", "a@x>\r\nQUIT", "<a@x>", &"x".repeat(249)] {
            assert!(matches!(pool.body(id).await, Err(NntpError::InvalidMessageId { .. })), "{id:?}");
        }
        assert_eq!(server.logins(), 0);
        assert!(matches!(Pool::new(Vec::new()).body("a@x").await, Err(NntpError::NoServers { .. })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn requests_pipeline_on_one_connection_and_fill_its_depth() {
        let server = FakeServer::start();
        let posting = posting();
        server.add(&posting);
        server.set_latency(Duration::from_millis(100));
        let pool = Pool::new(vec![server.config(1, 0)]);
        let ids: Vec<&str> = posting.ids().take(8).collect();
        let started = Instant::now();
        let bodies = futures_util::future::join_all(ids.iter().map(|id| pool.body(id))).await;
        assert!(bodies.iter().all(Result::is_ok));
        // Two rounds of four in flight, not eight round trips.
        assert!(started.elapsed() < Duration::from_millis(600), "{:?}", started.elapsed());
        assert_eq!((server.logins(), server.most_in_flight()), (1, 4));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_takes_an_idle_connection_before_queueing_behind_a_slow_one() {
        let server = FakeServer::start();
        let posting = posting();
        server.add(&posting);
        let ids: Vec<&str> = posting.ids().collect();
        let pool = Pool::new(vec![server.config(2, 0)]);
        // Open both connections: with replies held back, the first request is still in flight
        // when the others arrive, so the pool opens the second.
        server.set_latency(Duration::from_millis(50));
        futures_util::future::join_all(ids[..8].iter().map(|id| pool.body(id))).await;
        server.set_latency(Duration::ZERO);
        assert_eq!(pool.stats().connections, 2);
        server.delay(ids[0], Duration::from_secs(2));
        let slow = pool.body(ids[0]);
        let fast = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let started = Instant::now();
            pool.body(ids[1]).await.unwrap();
            started.elapsed()
        };
        let (slow, fast) = tokio::join!(slow, fast);
        slow.unwrap();
        assert!(fast < Duration::from_millis(500), "{fast:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_connection_retries_every_request_that_was_on_it() {
        let server = FakeServer::start();
        let posting = posting();
        server.add(&posting);
        let ids: Vec<&str> = posting.ids().take(4).collect();
        server.cut(ids[1]);
        let pool = Pool::new(vec![server.config(1, 0)]);
        let bodies = futures_util::future::join_all(ids.iter().map(|id| pool.body(id))).await;
        assert!(bodies[0].is_ok(), "{:?}", bodies[0]);
        assert!(matches!(bodies[1], Err(NntpError::Unreachable { .. })), "{:?}", bodies[1]);
        assert!(bodies[2..].iter().all(Result::is_ok), "{bodies:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_caller_that_gives_up_leaves_the_connection_usable() {
        let server = FakeServer::start();
        let posting = posting();
        server.add(&posting);
        let ids: Vec<&str> = posting.ids().collect();
        server.set_latency(Duration::from_millis(200));
        let pool = Pool::new(vec![server.config(1, 0)]);
        for id in &ids[..6] {
            let _ = tokio::time::timeout(Duration::from_millis(20), pool.body(id)).await;
        }
        let bodies = futures_util::future::join_all(ids[6..14].iter().map(|id| pool.body(id))).await;
        assert!(bodies.iter().all(Result::is_ok), "{bodies:?}");
        assert_eq!(server.logins(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn background_requests_wait_for_the_limit_and_urgent_ones_do_not() {
        let server = FakeServer::start();
        let posting = posting();
        server.add(&posting);
        server.set_latency(Duration::from_millis(200));
        let pool = Pool::new(vec![server.config(8, 0)]);
        assert_eq!(pool.priority(), Priority::Background);
        let limit = pool.stats().background_limit as usize;
        assert!(limit < pool.capacity(), "{limit}");
        let ids: Vec<&str> = posting.ids().collect();
        let started = Instant::now();
        let background = futures_util::future::join_all(ids[..4 * limit].iter().map(|id| pool.body(id)));
        let seek = pool.with_priority(Priority::Urgent);
        let urgent = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let t = Instant::now();
            seek.body(ids[4 * limit]).await.unwrap();
            t.elapsed()
        };
        let (bodies, urgent) = tokio::join!(background, urgent);
        assert!(bodies.iter().all(Result::is_ok));
        // Four rounds for the background requests, at most two for the urgent one.
        assert!(started.elapsed() >= Duration::from_millis(780), "{:?}", started.elapsed());
        assert!(urgent < Duration::from_millis(500), "{urgent:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_urgent_request_takes_the_next_free_slot_before_waiting_background_ones() {
        let server = FakeServer::start();
        let posting = posting();
        server.add(&posting);
        server.set_latency(Duration::from_millis(100));
        let mut config = server.config(1, 0);
        config.pipeline = NonZeroU8::MIN;
        let pool = Pool::new(vec![config]);
        let ids: Vec<&str> = posting.ids().collect();
        let order = Mutex::new(Vec::new());
        let fetch = async |pool: Pool, k: usize, after: u64| {
            tokio::time::sleep(Duration::from_millis(after)).await;
            pool.body(ids[k]).await.unwrap();
            order.lock().push(k);
        };
        let seek = pool.with_priority(Priority::Urgent);
        tokio::join!(
            fetch(pool.clone(), 0, 0),
            fetch(pool.clone(), 1, 10),
            fetch(pool.clone(), 2, 10),
            fetch(seek, 3, 30)
        );
        let order = order.into_inner();
        assert_eq!((order[0], order[1]), (0, 3), "{order:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_the_pool_says_quit() {
        let server = FakeServer::start();
        let posting = posting();
        server.add(&posting);
        let pool = Pool::new(vec![server.config(1, 0)]);
        pool.body(posting.ids().next().unwrap()).await.unwrap();
        drop(pool);
        let deadline = Instant::now() + Duration::from_secs(5);
        while server.quits() == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(server.quits(), 1);
    }
}
