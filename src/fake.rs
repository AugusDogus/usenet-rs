//! A loopback NNTP server that serves articles from memory, and postings to fill it.
//! For tests, in this crate and in the ones that use it.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::nntp::Server;
use crate::yenc;

/// The username [`FakeServer`] accepts.
pub const USERNAME: &str = "tester";
/// The password [`FakeServer`] accepts.
pub const PASSWORD: &str = "letmein";

/// A release as posted: its NZB, and the article bodies the NZB lists.
#[derive(Clone, Debug)]
pub struct Posting {
    /// The NZB listing the posting's articles.
    pub nzb: Vec<u8>,
    /// Each posted file's name and the message-IDs of its articles, in order.
    pub files: Vec<(String, Vec<String>)>,
    articles: Vec<(String, Vec<u8>)>,
}

impl Posting {
    /// Posts `files` as they are, `part_size` bytes per article. Message-IDs derive from the
    /// title and the files, so two different postings never share an article.
    pub fn new(title: &str, files: &[(&str, &[u8])], part_size: usize) -> Self {
        assert!(part_size > 0, "articles hold at least one byte");
        let mut digest = crc32fast::Hasher::new();
        digest.update(title.as_bytes());
        for (name, data) in files {
            digest.update(name.as_bytes());
            digest.update(data);
        }
        let tag = format!("{:08x}{:x}", digest.finalize(), files.iter().map(|(_, d)| d.len()).sum::<usize>());
        let mut nzb = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        nzb.push_str("<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n");
        nzb.push_str(&format!("<head><meta type=\"name\">{}</meta></head>\n", escape(title)));
        let (mut posted, mut articles) = (Vec::new(), Vec::new());
        for (k, (name, data)) in files.iter().enumerate() {
            let parts = data.len().div_ceil(part_size).max(1);
            let subject = format!("{title} [{}/{}] - \"{name}\" yEnc (1/{parts})", k + 1, files.len());
            nzb.push_str(&format!(
                "<file poster=\"tester@example.invalid\" date=\"1700000000\" subject=\"{}\">\n",
                escape(&subject)
            ));
            nzb.push_str("<groups><group>alt.binaries.test</group></groups>\n<segments>\n");
            let mut ids = Vec::new();
            for i in 0..parts {
                let begin = i * part_size;
                let chunk = &data[begin..(begin + part_size).min(data.len())];
                let id = format!("{tag}.{k}.{i}@fake.invalid");
                let body = yenc::encode(chunk, name, data.len() as u64, begin as u64, Some(i as u32 + 1));
                nzb.push_str(&format!(
                    "<segment bytes=\"{}\" number=\"{}\">{}</segment>\n",
                    body.len(),
                    i + 1,
                    escape(&id)
                ));
                articles.push((id.clone(), body));
                ids.push(id);
            }
            nzb.push_str("</segments>\n</file>\n");
            posted.push((name.to_string(), ids));
        }
        nzb.push_str("</nzb>\n");
        Self { nzb: nzb.into_bytes(), files: posted, articles }
    }

    /// Stores `data` as `name` in RAR5 volumes holding at most `volume_data` bytes of it each,
    /// posted as `{title}.partNN.rar`, with a 1 KB sample stored ahead of it in the first.
    pub fn stored_rar(title: &str, name: &str, data: &[u8], volume_data: usize, part_size: usize) -> Self {
        use crate::rar::fixtures::{File, rar5};
        let chunks: Vec<&[u8]> = data.chunks(volume_data).collect();
        let mut volumes = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let split = (if i > 0 { 0x8 } else { 0 }) | (if i + 1 < chunks.len() { 0x10 } else { 0 });
            let movie = File { name, data: chunk.len() as u64, split, method: 0, unpacked: data.len() as u64 };
            let sample = File { name: "Sample/sample.mkv", data: 1024, split: 0, method: 0, unpacked: 1024 };
            let mut volume = if i == 0 { rar5(0, &[sample, movie], 0x5a) } else { rar5(i as u64, &[movie], 0) };
            let at = volume.len() - chunk.len();
            volume[at..].copy_from_slice(chunk);
            volumes.push((format!("{title}.part{:02}.rar", i + 1), volume));
        }
        let files: Vec<(&str, &[u8])> = volumes.iter().map(|(n, v)| (n.as_str(), v.as_slice())).collect();
        Self::new(title, &files, part_size)
    }

    /// Every message-ID in the posting.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.articles.iter().map(|(id, _)| id.as_str())
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[derive(Default)]
struct State {
    articles: HashMap<String, Arc<Vec<u8>>>,
    served: HashMap<String, u32>,
    /// Extra time before the reply to a command for this article.
    delays: HashMap<String, Duration>,
    /// Articles whose body is cut off halfway by closing the connection.
    cuts: HashSet<String>,
}

struct Inner {
    state: Mutex<State>,
    /// Bytes per second across all connections; 0 is unlimited.
    rate: AtomicU64,
    /// How long each BODY and STAT reply takes after its command arrives.
    latency: Mutex<Duration>,
    down: AtomicBool,
    /// When the shared link is next free, for the rate limit.
    link_free: Mutex<Instant>,
    logins: AtomicU64,
    quits: AtomicU64,
    /// Connections open now, and the most allowed at once; 0 is unlimited.
    sessions: AtomicU64,
    max_sessions: AtomicU64,
    /// The most commands one connection had waiting for their replies at once.
    most_in_flight: AtomicU64,
}

/// An NNTP server on 127.0.0.1 without TLS. It lives until the process exits.
///
/// Each connection reads commands as they arrive and answers them in order, so a client can
/// pipeline them; with [`set_latency`](Self::set_latency), replies overlap as they would over
/// a long link.
#[derive(Clone)]
pub struct FakeServer {
    inner: Arc<Inner>,
    port: u16,
}

impl Default for FakeServer {
    fn default() -> Self {
        Self::start()
    }
}

impl FakeServer {
    /// Starts a server on a free loopback port.
    ///
    /// # Panics
    ///
    /// When no loopback port can be bound; this is a test fixture.
    pub fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port is free");
        let port = listener.local_addr().expect("a bound socket has an address").port();
        let inner = Arc::new(Inner {
            state: Mutex::default(),
            rate: AtomicU64::new(0),
            latency: Mutex::new(Duration::ZERO),
            down: AtomicBool::new(false),
            link_free: Mutex::new(Instant::now()),
            logins: AtomicU64::new(0),
            quits: AtomicU64::new(0),
            sessions: AtomicU64::new(0),
            max_sessions: AtomicU64::new(0),
            most_in_flight: AtomicU64::new(0),
        });
        let shared = inner.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let inner = shared.clone();
                std::thread::spawn(move || {
                    let open = inner.sessions.fetch_add(1, Ordering::SeqCst) + 1;
                    let max = inner.max_sessions.load(Ordering::SeqCst);
                    let _ = if max > 0 && open > max {
                        (&stream).write_all(b"502 too many connections\r\n")
                    } else {
                        serve(&inner, stream)
                    };
                    inner.sessions.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Self { inner, port }
    }

    /// The port the server listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// How to reach this server, with `connections` at most.
    pub fn config(&self, connections: u16, priority: i32) -> Server {
        Server {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port: self.port,
            tls: false,
            username: USERNAME.into(),
            password: PASSWORD.into(),
            connections,
            priority,
            pipeline: Server::DEFAULT_PIPELINE,
        }
    }

    /// Serves the posting's articles.
    pub fn add(&self, posting: &Posting) {
        let mut state = self.inner.state.lock();
        for (id, body) in &posting.articles {
            state.articles.insert(id.clone(), Arc::new(body.clone()));
        }
    }

    /// Takes an article down, as a provider does on a takedown notice.
    pub fn remove(&self, message_id: &str) {
        self.inner.state.lock().articles.remove(message_id);
    }

    /// How many times the article's body was sent.
    pub fn served(&self, message_id: &str) -> u32 {
        self.inner.state.lock().served.get(message_id).copied().unwrap_or(0)
    }

    /// How many article bodies were sent in all.
    pub fn bodies(&self) -> u64 {
        self.inner.state.lock().served.values().map(|&n| u64::from(n)).sum()
    }

    /// Logins accepted, one per connection opened.
    pub fn logins(&self) -> u64 {
        self.inner.logins.load(Ordering::Relaxed)
    }

    /// QUIT commands received.
    pub fn quits(&self) -> u64 {
        self.inner.quits.load(Ordering::Relaxed)
    }

    /// The most commands one connection has had waiting for replies at once: 1 for a client
    /// that does not pipeline.
    pub fn most_in_flight(&self) -> u64 {
        self.inner.most_in_flight.load(Ordering::Relaxed)
    }

    /// Limits what all connections send together, in bytes per second. 0 is unlimited.
    pub fn set_rate(&self, bytes_per_second: u64) {
        self.inner.rate.store(bytes_per_second, Ordering::Relaxed);
    }

    /// Delays each BODY and STAT reply by `latency` after its command arrives, as a distant
    /// server does. Commands that arrive together are delayed together, as over a network.
    pub fn set_latency(&self, latency: Duration) {
        *self.inner.latency.lock() = latency;
    }

    /// Delays replies about one article by `extra` more, on top of the latency.
    pub fn delay(&self, message_id: &str, extra: Duration) {
        self.inner.state.lock().delays.insert(message_id.to_string(), extra);
    }

    /// Closes the connection halfway through the article's body, every time it is asked for.
    pub fn cut(&self, message_id: &str) {
        self.inner.state.lock().cuts.insert(message_id.to_string());
    }

    /// Refuses connections beyond `max` open at once, as a provider does past an account's
    /// connection limit. 0 is unlimited.
    pub fn set_max_sessions(&self, max: u64) {
        self.inner.max_sessions.store(max, Ordering::SeqCst);
    }

    /// A server that is down refuses new connections and drops open ones at their next command.
    pub fn set_down(&self, down: bool) {
        self.inner.down.store(down, Ordering::Relaxed);
    }
}

/// A reply, when to send it, and what to do after.
struct Outgoing {
    due: Instant,
    bytes: Vec<u8>,
    then: Then,
}

enum Then {
    Continue,
    /// Send this many bytes of the reply, then close the connection.
    Cut(usize),
    Close,
}

/// Commands waiting for replies on one connection.
struct InFlight(AtomicU64);

fn serve(inner: &Arc<Inner>, stream: TcpStream) -> std::io::Result<()> {
    // As a real server does, or a reply's last piece waits on the client's delayed ACK.
    stream.set_nodelay(true)?;
    let mut out = stream.try_clone()?;
    if inner.down.load(Ordering::Relaxed) {
        return out.write_all(b"502 service unavailable\r\n");
    }
    out.write_all(b"200 fake NNTP ready\r\n")?;
    let in_flight = Arc::new(InFlight(AtomicU64::new(0)));
    // Bounded, so a client that never reads cannot make the server buffer without end.
    let (replies, queue) = mpsc::sync_channel::<Outgoing>(64);
    let writer = {
        let (inner, in_flight) = (inner.clone(), in_flight.clone());
        std::thread::spawn(move || write_replies(&inner, out, &queue, &in_flight))
    };
    let result = read_commands(inner, stream, &replies, &in_flight);
    drop(replies);
    let _ = writer.join();
    result
}

fn read_commands(
    inner: &Inner,
    stream: TcpStream,
    replies: &mpsc::SyncSender<Outgoing>,
    in_flight: &InFlight,
) -> std::io::Result<()> {
    let mut lines = BufReader::new(stream);
    let (mut user, mut authed) = (String::new(), false);
    let mut line = String::new();
    loop {
        line.clear();
        if lines.read_line(&mut line)? == 0 || inner.down.load(Ordering::Relaxed) {
            return Ok(());
        }
        let arrived = Instant::now();
        let waiting = in_flight.0.fetch_add(1, Ordering::SeqCst) + 1;
        inner.most_in_flight.fetch_max(waiting, Ordering::SeqCst);
        let words: Vec<&str> = line.split_whitespace().collect();
        let verb = words.first().map(|w| w.to_ascii_uppercase()).unwrap_or_default();
        let arg = |i: usize| words.get(i).copied().unwrap_or_default();
        let id = arg(1).trim_start_matches('<').trim_end_matches('>').to_string();
        let now = |bytes: &[u8]| Outgoing { due: arrived, bytes: bytes.to_vec(), then: Then::Continue };
        let later = |bytes: Vec<u8>, then| {
            let delay = inner.state.lock().delays.get(&id).copied().unwrap_or_default();
            Outgoing { due: arrived + *inner.latency.lock() + delay, bytes, then }
        };
        let reply = match verb.as_str() {
            "AUTHINFO" if arg(1).eq_ignore_ascii_case("USER") => {
                user = arg(2).to_string();
                now(b"381 password required\r\n")
            }
            "AUTHINFO" if arg(1).eq_ignore_ascii_case("PASS") => {
                authed = user == USERNAME && arg(2) == PASSWORD;
                if authed {
                    inner.logins.fetch_add(1, Ordering::Relaxed);
                    now(b"281 welcome\r\n")
                } else {
                    now(b"481 authentication failed\r\n")
                }
            }
            "BODY" | "STAT" if !authed => now(b"480 authentication required\r\n"),
            "STAT" => {
                let found = inner.state.lock().articles.contains_key(&id);
                let reply = if found { format!("223 0 <{id}>\r\n") } else { "430 no such article\r\n".into() };
                later(reply.into_bytes(), Then::Continue)
            }
            "BODY" => {
                let (body, cut) = {
                    let mut state = inner.state.lock();
                    let body = state.articles.get(&id).cloned();
                    if body.is_some() {
                        *state.served.entry(id.clone()).or_default() += 1;
                    }
                    (body, state.cuts.contains(&id))
                };
                match body {
                    Some(body) => {
                        let mut reply = format!("222 0 <{id}>\r\n").into_bytes();
                        for l in body.split_inclusive(|&b| b == b'\n') {
                            if l.first() == Some(&b'.') {
                                reply.push(b'.');
                            }
                            reply.extend_from_slice(l);
                        }
                        reply.extend_from_slice(b".\r\n");
                        let then = if cut { Then::Cut(reply.len() / 2) } else { Then::Continue };
                        later(reply, then)
                    }
                    None => later(b"430 no such article\r\n".to_vec(), Then::Continue),
                }
            }
            "QUIT" => {
                inner.quits.fetch_add(1, Ordering::Relaxed);
                Outgoing { due: arrived, bytes: b"205 bye\r\n".to_vec(), then: Then::Close }
            }
            _ => now(b"500 unknown command\r\n"),
        };
        let close = !matches!(reply.then, Then::Continue);
        if replies.send(reply).is_err() {
            return Ok(());
        }
        if close {
            // Read on to the client's close, so the socket shuts with a FIN, not a reset that
            // could discard replies the client has not read yet.
            std::io::copy(&mut lines, &mut std::io::sink())?;
            return Ok(());
        }
    }
}

fn write_replies(inner: &Inner, mut out: TcpStream, queue: &mpsc::Receiver<Outgoing>, in_flight: &InFlight) {
    for reply in queue {
        std::thread::sleep(reply.due.saturating_duration_since(Instant::now()));
        let bytes = match reply.then {
            Then::Cut(at) => &reply.bytes[..at],
            Then::Continue | Then::Close => &reply.bytes[..],
        };
        let sent = send(inner, &mut out, bytes);
        in_flight.0.fetch_sub(1, Ordering::SeqCst);
        if sent.is_err() || !matches!(reply.then, Then::Continue) {
            let _ = out.shutdown(Shutdown::Write);
            return;
        }
    }
}

fn send(inner: &Inner, out: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    for chunk in bytes.chunks(16 * 1024) {
        let rate = inner.rate.load(Ordering::Relaxed);
        if rate > 0 {
            let cost = Duration::from_secs_f64(chunk.len() as f64 / rate as f64);
            let done = {
                let mut free = inner.link_free.lock();
                *free = (*free).max(Instant::now()) + cost;
                *free
            };
            std::thread::sleep(done.saturating_duration_since(Instant::now()));
        }
        out.write_all(chunk)?;
    }
    Ok(())
}
