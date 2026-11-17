//! One logged-in connection, driven by its own task: commands go out as they come in, up to
//! the depth the pool allows, and replies are matched to them in order.

use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream, lookup_host};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until, timeout};

use super::reply::{Expect, MAX_BODY, Replies, Reply, Status};
use super::{ConnectFailure, IoFailure, NntpError, Server, Violation};

/// Silence this long while a reply is due means the connection is dead.
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(20);
/// Connecting, TLS, greeting, and login. A server at its connection limit can accept the
/// socket and then say nothing, so this is short; the pool moves on to another server.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// How long QUIT may take when a connection is closed on purpose.
const QUIT_TIMEOUT: Duration = Duration::from_secs(1);
/// The socket receive buffer asked for: a 256 KB article per round trip at most is a slow
/// start on a distant server, so room for a few articles in flight.
const RECEIVE_BUFFER: u32 = 4 * 1024 * 1024;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// How a request ended on one connection.
#[derive(Debug)]
pub(crate) enum Outcome<T> {
    Found(T),
    Missing,
    /// The connection failed on this command's reply.
    Failed(NntpError),
    /// The connection failed before this command's reply began, on one ahead of it.
    Dropped,
}

/// A command for a connection, and where its answer goes.
#[derive(Debug)]
pub(crate) enum Job {
    Body { message_id: String, reply: oneshot::Sender<Outcome<Vec<u8>>> },
    Stat { message_id: String, reply: oneshot::Sender<Outcome<()>> },
}

impl Job {
    fn expect(&self) -> Expect {
        match self {
            Self::Body { .. } => Expect::Body,
            Self::Stat { .. } => Expect::Line,
        }
    }

    fn write_command(&self, out: &mut Vec<u8>) {
        let (verb, id) = match self {
            Self::Body { message_id, .. } => ("BODY", message_id),
            Self::Stat { message_id, .. } => ("STAT", message_id),
        };
        out.extend_from_slice(verb.as_bytes());
        out.extend_from_slice(b" <");
        out.extend_from_slice(id.as_bytes());
        out.extend_from_slice(b">\r\n");
    }

    /// Tells the caller the connection failed, on this command's reply if `own`.
    fn fail(self, error: NntpError, own: bool) {
        fn outcome<T>(error: NntpError, own: bool) -> Outcome<T> {
            if own { Outcome::Failed(error) } else { Outcome::Dropped }
        }
        // A caller that gave up has dropped its receiver; nobody is left to tell.
        let _ = match self {
            Self::Body { reply, .. } => reply.send(outcome(error, own)).map_err(drop),
            Self::Stat { reply, .. } => reply.send(outcome(error, own)).map_err(drop),
        };
    }

    /// Hands the reply to the caller. An error means the connection cannot go on.
    fn settle(self, reply: Reply, server: &str) -> Result<(), NntpError> {
        let unexpected = |command, reply: String| NntpError::Reply { server: server.to_string(), command, reply };
        let missing = |status: &Status| matches!(status.code, Some(430 | 423));
        let (sent, error) = match (self, reply) {
            (Self::Body { reply, .. }, Reply::Body(body)) => (reply.send(Outcome::Found(body)).is_ok(), None),
            (Self::Body { reply, .. }, Reply::Line(status)) if missing(&status) => {
                (reply.send(Outcome::Missing).is_ok(), None)
            }
            (Self::Body { reply, .. }, Reply::Line(status)) => {
                let error = unexpected("BODY", status.text);
                (reply.send(Outcome::Failed(error.clone())).is_ok(), Some(error))
            }
            (Self::Stat { reply, .. }, Reply::Line(status)) if status.code == Some(223) => {
                (reply.send(Outcome::Found(())).is_ok(), None)
            }
            (Self::Stat { reply, .. }, Reply::Line(status)) if missing(&status) => {
                (reply.send(Outcome::Missing).is_ok(), None)
            }
            (Self::Stat { reply, .. }, Reply::Line(status)) => {
                let error = unexpected("STAT", status.text);
                (reply.send(Outcome::Failed(error.clone())).is_ok(), Some(error))
            }
            // A line was expected, so the parser never reads a body for STAT.
            (Self::Stat { reply, .. }, Reply::Body(_)) => {
                let error = unexpected("STAT", "a multiline body".into());
                (reply.send(Outcome::Failed(error.clone())).is_ok(), Some(error))
            }
        };
        // A caller that gave up is not an error; the reply was read, so the stream is in step.
        let _ = sent;
        error.map_or(Ok(()), Err)
    }
}

/// What woke a connection's task.
enum Event {
    /// A command from the pool, or None once the pool lets go.
    Job(Option<Job>),
    Read(Result<(), IoFailure>),
    /// A reply is overdue.
    Silent,
}

/// What the connection's task tells the pool. Dropping it tells the pool the connection is
/// gone, whether it failed, was closed, or its task was cancelled.
pub(crate) trait Owner: Send + 'static {
    /// One reply came back, so one fewer command is in flight.
    fn answered(&self);
}

/// A connection that has logged in and is ready for commands.
pub(crate) struct Session {
    io: Box<dyn Io>,
    replies: Replies,
    label: String,
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            use rustls_platform_verifier::ConfigVerifierExt;
            rustls::ClientConfig::with_platform_verifier().map(Arc::new).map_err(|e| e.to_string())
        })
        .clone()
}

/// A TCP connection to the first address of `host` that takes one, with Nagle off, since
/// commands are small and a reply waits on each.
async fn tcp(host: &str, port: u16) -> Result<TcpStream, IoFailure> {
    let mut last = IoFailure::Os { kind: std::io::ErrorKind::NotFound, message: format!("{host} has no address") };
    for address in lookup_host((host, port)).await? {
        let socket = if address.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
        // Only a hint; the system may grant less, and that is fine.
        let _ = socket.set_recv_buffer_size(RECEIVE_BUFFER);
        match socket.connect(address).await {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                return Ok(stream);
            }
            Err(e) => last = e.into(),
        }
    }
    Err(last)
}

impl Session {
    /// Connects and logs in, within [`CONNECT_TIMEOUT`].
    pub async fn open(server: &Server) -> Result<Self, NntpError> {
        let label = format!("{}:{}", server.host, server.port);
        let seconds = CONNECT_TIMEOUT.as_secs();
        timeout(CONNECT_TIMEOUT, Self::connect(server, label.clone()))
            .await
            .map_err(|_| NntpError::Connect { server: label, failure: ConnectFailure::TimedOut { seconds } })?
    }

    async fn connect(server: &Server, label: String) -> Result<Self, NntpError> {
        let fail = |failure| NntpError::Connect { server: label.clone(), failure };
        let tcp = tcp(&server.host, server.port).await.map_err(|e| fail(ConnectFailure::Tcp(e)))?;
        let io: Box<dyn Io> = if server.tls {
            let name = rustls::pki_types::ServerName::try_from(server.host.clone())
                .map_err(|_| fail(ConnectFailure::HostName))?;
            let config = tls_config().map_err(|e| fail(ConnectFailure::Verifier(e)))?;
            let stream = tokio_rustls::TlsConnector::from(config)
                .connect(name, tcp)
                .await
                .map_err(|e| fail(ConnectFailure::Tls(e.into())))?;
            Box::new(stream)
        } else {
            Box::new(tcp)
        };
        let mut session = Self { io, replies: Replies::new(MAX_BODY), label };
        let greeting = session.line().await?;
        if !matches!(greeting.code, Some(200 | 201)) {
            return Err(NntpError::Refused { server: session.label, reply: greeting.text });
        }
        let user = session.command(&format!("AUTHINFO USER {}", server.username)).await?;
        let login = if user.code == Some(381) {
            session.command(&format!("AUTHINFO PASS {}", server.password)).await?
        } else {
            user
        };
        if login.code != Some(281) {
            return Err(NntpError::Login { server: session.label, reply: login.text });
        }
        Ok(session)
    }

    fn io_error(&self, failure: IoFailure) -> NntpError {
        NntpError::Io { server: self.label.clone(), failure }
    }

    fn protocol(&self, violation: Violation) -> NntpError {
        NntpError::Protocol { server: self.label.clone(), violation }
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<(), IoFailure> {
        let sent = timeout(IO_TIMEOUT, async {
            self.io.write_all(bytes).await?;
            self.io.flush().await
        });
        match sent.await {
            Ok(result) => Ok(result?),
            Err(_) => Err(IoFailure::TimedOut { seconds: IO_TIMEOUT.as_secs() }),
        }
    }

    /// Reads once, whatever has arrived. Cancel-safe: nothing is lost if it never finishes.
    async fn fill(&mut self) -> Result<(), IoFailure> {
        match self.io.read_buf(self.replies.buffer()).await {
            Ok(0) => Err(IoFailure::Closed),
            Ok(_) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// One status line, for the login before commands are pipelined.
    async fn line(&mut self) -> Result<Status, NntpError> {
        loop {
            match self.replies.next(Expect::Line) {
                Ok(Some(Reply::Line(status))) => return Ok(status),
                Ok(Some(Reply::Body(_))) => return Err(self.protocol(Violation::Unsolicited)),
                Ok(None) => {}
                Err(violation) => return Err(self.protocol(violation)),
            }
            match timeout(IO_TIMEOUT, self.fill()).await {
                Ok(Ok(())) => {}
                Ok(Err(failure)) => return Err(self.io_error(failure)),
                Err(_) => return Err(self.io_error(IoFailure::TimedOut { seconds: IO_TIMEOUT.as_secs() })),
            }
        }
    }

    async fn command(&mut self, line: &str) -> Result<Status, NntpError> {
        let mut bytes = line.as_bytes().to_vec();
        bytes.extend_from_slice(b"\r\n");
        self.write(&bytes).await.map_err(|failure| self.io_error(failure))?;
        self.line().await
    }

    /// Says goodbye, briefly; the server drops the session either way.
    async fn quit(mut self) {
        let _ = timeout(QUIT_TIMEOUT, async {
            if self.io.write_all(b"QUIT\r\n").await.is_ok() {
                let _ = self.io.flush().await;
            }
            let _ = self.io.shutdown().await;
        })
        .await;
    }

    /// Hands every whole reply that has arrived to the job it answers, in order.
    fn answer(&mut self, waiting: &mut VecDeque<Job>, owner: &impl Owner) -> Result<(), NntpError> {
        loop {
            let Some(front) = waiting.front() else {
                return if self.replies.is_idle() { Ok(()) } else { Err(self.protocol(Violation::Unsolicited)) };
            };
            match self.replies.next(front.expect()) {
                Ok(None) => return Ok(()),
                Ok(Some(reply)) => {
                    let Some(job) = waiting.pop_front() else { return Ok(()) };
                    owner.answered();
                    job.settle(reply, &self.label)?;
                }
                Err(violation) => return Err(self.protocol(violation)),
            }
        }
    }

    /// Serves `jobs` until the pool lets go of the connection or it fails. On failure every
    /// command in flight, and every one queued behind them, fails with the same error, so
    /// the pool can retry each elsewhere; only the one whose reply was due is to blame.
    pub async fn drive(mut self, mut jobs: mpsc::UnboundedReceiver<Job>, owner: impl Owner) {
        let mut waiting = VecDeque::new();
        let mut commands = Vec::new();
        let mut deadline = Instant::now();
        let error = loop {
            let event = tokio::select! {
                job = jobs.recv() => Event::Job(job),
                read = self.fill() => Event::Read(read),
                () = sleep_until(deadline), if !waiting.is_empty() => Event::Silent,
            };
            match event {
                // The pool is gone; nobody waits on what is in flight.
                Event::Job(None) => return self.quit().await,
                Event::Job(Some(job)) => {
                    if waiting.is_empty() {
                        deadline = Instant::now() + IO_TIMEOUT;
                    }
                    commands.clear();
                    job.write_command(&mut commands);
                    waiting.push_back(job);
                    // The pool bounds what is queued, so this sends at most the depth at once.
                    while let Ok(job) = jobs.try_recv() {
                        job.write_command(&mut commands);
                        waiting.push_back(job);
                    }
                    if let Err(failure) = self.write(&commands).await {
                        break self.io_error(failure);
                    }
                }
                Event::Read(Err(failure)) => break self.io_error(failure),
                Event::Read(Ok(())) => {
                    deadline = Instant::now() + IO_TIMEOUT;
                    if let Err(error) = self.answer(&mut waiting, &owner) {
                        break error;
                    }
                }
                Event::Silent => break self.io_error(IoFailure::TimedOut { seconds: IO_TIMEOUT.as_secs() }),
            }
        };
        // Leave the pool first, so nothing more is queued while the rest is failed.
        drop(owner);
        jobs.close();
        // A command answered with an unexpected reply was failed as it was answered.
        let mut own = !matches!(error, NntpError::Reply { .. });
        for job in waiting.drain(..) {
            job.fail(error.clone(), own);
            own = false;
        }
        while let Ok(job) = jobs.try_recv() {
            job.fail(error.clone(), false);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::TcpListener;

    /// A server that greets, accepts any login, and answers each later command with the
    /// next of `replies`, raw, then hangs up.
    pub async fn scripted(replies: Vec<Vec<u8>>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            write.write_all(b"200 hi\r\n").await.unwrap();
            for login in ["381 more\r\n", "281 ok\r\n"] {
                lines.next_line().await.unwrap();
                write.write_all(login.as_bytes()).await.unwrap();
            }
            for reply in replies {
                if lines.next_line().await.ok().flatten().is_none() {
                    return;
                }
                if write.write_all(&reply).await.is_err() {
                    return;
                }
            }
        });
        port
    }

    pub fn local(port: u16, connections: u16) -> Server {
        Server {
            host: "127.0.0.1".into(),
            port,
            tls: false,
            username: "u".into(),
            password: "p".into(),
            connections,
            priority: 0,
            pipeline: Server::DEFAULT_PIPELINE,
        }
    }

    struct Counted(Arc<std::sync::atomic::AtomicUsize>);

    impl Owner for Counted {
        fn answered(&self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn body(id: &str) -> (Job, oneshot::Receiver<Outcome<Vec<u8>>>) {
        let (reply, rx) = oneshot::channel();
        (Job::Body { message_id: id.into(), reply }, rx)
    }

    #[tokio::test]
    async fn pipelined_replies_go_to_their_commands_in_order() {
        let replies =
            vec![b"222 0 <a>\r\na\r\n.\r\n".to_vec(), b"430 no\r\n".to_vec(), b"222 0 <c>\r\n..c\r\n.\r\n".to_vec()];
        let session = Session::open(&local(scripted(replies).await, 1)).await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let answered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(session.drive(rx, Counted(answered.clone())));
        let (jobs, receivers): (Vec<_>, Vec<_>) = ["a", "b", "c"].into_iter().map(body).unzip();
        for job in jobs {
            tx.send(job).unwrap();
        }
        let mut got = Vec::new();
        for rx in receivers {
            got.push(match rx.await.unwrap() {
                Outcome::Found(body) => String::from_utf8(body).unwrap(),
                Outcome::Missing => "missing".into(),
                Outcome::Failed(e) => e.to_string(),
                Outcome::Dropped => "dropped".into(),
            });
        }
        assert_eq!(got, ["a\r\n", "missing", ".c\r\n"]);
        assert_eq!(answered.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_broken_connection_fails_every_command_in_flight() {
        let replies = vec![b"222 0 <a>\r\na\r\n.\r\n".to_vec(), b"500 what\r\n".to_vec()];
        let session = Session::open(&local(scripted(replies).await, 1)).await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let answered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(session.drive(rx, Counted(answered.clone())));
        let (jobs, receivers): (Vec<_>, Vec<_>) = ["a", "b", "c", "d"].into_iter().map(body).unzip();
        for job in jobs {
            tx.send(job).unwrap();
        }
        let mut got = Vec::new();
        for rx in receivers {
            got.push(rx.await.unwrap());
        }
        assert!(matches!(got[0], Outcome::Found(_)), "{got:?}");
        assert!(matches!(got[1], Outcome::Failed(NntpError::Reply { command: "BODY", .. })), "{got:?}");
        assert!(got[2..].iter().all(|o| matches!(o, Outcome::Dropped)));
        assert!(tx.send(body("e").0).is_err(), "a failed connection takes no more commands");
    }

    #[tokio::test]
    async fn a_reply_that_is_not_ascii_is_an_error_not_a_panic() {
        for reply in ["22\u{20ac}x\r\n", "2\u{20ac}\r\n", "\r\n", "2\r\n", "\x01\x02\x03\r\n", "999 what\r\n"] {
            let session = Session::open(&local(scripted(vec![reply.as_bytes().to_vec()]).await, 1)).await.unwrap();
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(session.drive(rx, Counted(Default::default())));
            let (job, receiver) = body("a@x");
            tx.send(job).unwrap();
            assert!(matches!(receiver.await.unwrap(), Outcome::Failed(NntpError::Reply { .. })), "{reply:?}");
        }
    }

    #[tokio::test]
    async fn bytes_nobody_asked_for_drop_the_connection() {
        let replies = vec![b"223 0 <a>\r\ngarbage\r\n".to_vec()];
        let session = Session::open(&local(scripted(replies).await, 1)).await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(session.drive(rx, Counted(Default::default())));
        let (reply, receiver) = oneshot::channel();
        tx.send(Job::Stat { message_id: "a".into(), reply }).unwrap();
        assert!(matches!(receiver.await.unwrap(), Outcome::Found(())));
        handle.await.unwrap();
        assert!(tx.is_closed());
    }
}
