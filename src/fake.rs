//! A loopback NNTP server that serves articles from memory, and postings to fill it.
//! For tests, in this crate and in the ones that use it.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::nntp::Server;
use crate::yenc;

pub const USERNAME: &str = "tester";
pub const PASSWORD: &str = "letmein";

/// A release as posted: its NZB, and the article bodies the NZB lists.
#[derive(Clone, Debug)]
pub struct Posting {
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
}

struct Inner {
    state: Mutex<State>,
    /// Bytes per second across all connections; 0 is unlimited.
    rate: AtomicU64,
    down: AtomicBool,
    /// When the shared link is next free, for the rate limit.
    link_free: Mutex<Instant>,
    logins: AtomicU64,
    /// Connections open now, and the most allowed at once; 0 is unlimited.
    sessions: AtomicU64,
    max_sessions: AtomicU64,
}

/// An NNTP server on 127.0.0.1 without TLS. It lives until the process exits.
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
    pub fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port is free");
        let port = listener.local_addr().expect("a bound socket has an address").port();
        let inner = Arc::new(Inner {
            state: Mutex::default(),
            rate: AtomicU64::new(0),
            down: AtomicBool::new(false),
            link_free: Mutex::new(Instant::now()),
            logins: AtomicU64::new(0),
            sessions: AtomicU64::new(0),
            max_sessions: AtomicU64::new(0),
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
        }
    }

    pub fn add(&self, posting: &Posting) {
        let mut state = self.inner.state.lock().unwrap();
        for (id, body) in &posting.articles {
            state.articles.insert(id.clone(), Arc::new(body.clone()));
        }
    }

    /// Takes an article down, as a provider does on a takedown notice.
    pub fn remove(&self, message_id: &str) {
        self.inner.state.lock().unwrap().articles.remove(message_id);
    }

    /// How many times the article's body was sent.
    pub fn served(&self, message_id: &str) -> u32 {
        self.inner.state.lock().unwrap().served.get(message_id).copied().unwrap_or(0)
    }

    /// How many article bodies were sent in all.
    pub fn bodies(&self) -> u64 {
        self.inner.state.lock().unwrap().served.values().map(|&n| u64::from(n)).sum()
    }

    /// Logins accepted, one per connection opened.
    pub fn logins(&self) -> u64 {
        self.inner.logins.load(Ordering::Relaxed)
    }

    /// Limits what all connections send together, in bytes per second. 0 is unlimited.
    pub fn set_rate(&self, bytes_per_second: u64) {
        self.inner.rate.store(bytes_per_second, Ordering::Relaxed);
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

fn serve(inner: &Inner, stream: TcpStream) -> std::io::Result<()> {
    let mut out = stream.try_clone()?;
    if inner.down.load(Ordering::Relaxed) {
        return out.write_all(b"502 service unavailable\r\n");
    }
    out.write_all(b"200 fake NNTP ready\r\n")?;
    let mut lines = BufReader::new(stream);
    let (mut user, mut authed) = (String::new(), false);
    let mut line = String::new();
    loop {
        line.clear();
        if lines.read_line(&mut line)? == 0 || inner.down.load(Ordering::Relaxed) {
            return Ok(());
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let verb = words.first().map(|w| w.to_ascii_uppercase()).unwrap_or_default();
        let arg = |i: usize| words.get(i).copied().unwrap_or_default();
        let id = arg(1).trim_start_matches('<').trim_end_matches('>').to_string();
        match verb.as_str() {
            "AUTHINFO" if arg(1).eq_ignore_ascii_case("USER") => {
                user = arg(2).to_string();
                out.write_all(b"381 password required\r\n")?;
            }
            "AUTHINFO" if arg(1).eq_ignore_ascii_case("PASS") => {
                authed = user == USERNAME && arg(2) == PASSWORD;
                if authed {
                    inner.logins.fetch_add(1, Ordering::Relaxed);
                    out.write_all(b"281 welcome\r\n")?;
                } else {
                    out.write_all(b"481 authentication failed\r\n")?;
                }
            }
            "BODY" | "STAT" if !authed => out.write_all(b"480 authentication required\r\n")?,
            "STAT" => {
                let found = inner.state.lock().unwrap().articles.contains_key(&id);
                let reply = if found { format!("223 0 <{id}>\r\n") } else { "430 no such article\r\n".into() };
                out.write_all(reply.as_bytes())?;
            }
            "BODY" => {
                let body = {
                    let mut state = inner.state.lock().unwrap();
                    let body = state.articles.get(&id).cloned();
                    if body.is_some() {
                        *state.served.entry(id.clone()).or_default() += 1;
                    }
                    body
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
                        send(inner, &mut out, &reply)?;
                    }
                    None => out.write_all(b"430 no such article\r\n")?,
                }
            }
            "QUIT" => return out.write_all(b"205 bye\r\n"),
            _ => out.write_all(b"500 unknown command\r\n")?,
        }
    }
}

fn send(inner: &Inner, out: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    for chunk in bytes.chunks(16 * 1024) {
        let rate = inner.rate.load(Ordering::Relaxed);
        if rate > 0 {
            let cost = Duration::from_secs_f64(chunk.len() as f64 / rate as f64);
            let done = {
                let mut free = inner.link_free.lock().unwrap();
                *free = (*free).max(Instant::now()) + cost;
                *free
            };
            std::thread::sleep(done.saturating_duration_since(Instant::now()));
        }
        out.write_all(chunk)?;
    }
    Ok(())
}
