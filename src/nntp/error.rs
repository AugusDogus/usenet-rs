//! What can go wrong talking to news servers, and what to do about it.

use std::fmt;
use std::io;

use super::Violation;

/// Why a request to the pool, or one attempt of it on one server, failed.
///
/// [`Pool::body`](super::Pool::body) and [`Pool::stat`](super::Pool::stat) end in
/// [`Missing`](Self::Missing), [`Unreachable`](Self::Unreachable), [`NoServers`](Self::NoServers)
/// or [`InvalidMessageId`](Self::InvalidMessageId). The other variants describe one server's
/// failure, inside [`Unreachable`](Self::Unreachable) and in
/// [`ServerReport::last_error`](super::ServerReport::last_error).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NntpError {
    /// No configured server has the article; it expired or was taken down.
    Missing {
        /// The article asked for, without angle brackets.
        message_id: String,
    },
    /// The message-ID cannot be sent to a server: it is empty, longer than 250 bytes, or
    /// holds spaces, control characters or angle brackets.
    InvalidMessageId {
        /// The message-ID as given.
        message_id: String,
    },
    /// The pool was made without servers.
    NoServers {
        /// The article asked for.
        message_id: String,
    },
    /// No connection could be made: the host, the network, or TLS failed.
    Connect {
        /// The server, as `host:port`.
        server: String,
        /// What went wrong.
        failure: ConnectFailure,
    },
    /// The server answered the connection with something other than a greeting, usually
    /// because the account is at its connection limit or the server is down.
    Refused {
        /// The server, as `host:port`.
        server: String,
        /// The server's reply.
        reply: String,
    },
    /// The server did not accept the username and password.
    Login {
        /// The server, as `host:port`.
        server: String,
        /// The server's reply.
        reply: String,
    },
    /// The server answered a command with a reply it does not allow.
    Reply {
        /// The server, as `host:port`.
        server: String,
        /// The command answered: `BODY` or `STAT`.
        command: &'static str,
        /// The server's reply.
        reply: String,
    },
    /// An open connection failed.
    Io {
        /// The server, as `host:port`.
        server: String,
        /// What went wrong.
        failure: IoFailure,
    },
    /// The server sent something that is not NNTP.
    Protocol {
        /// The server, as `host:port`.
        server: String,
        /// What it sent.
        violation: Violation,
    },
    /// Every server failed for a reason other than lacking the article, or the servers that
    /// lacked it were joined by ones that failed, so the article may still exist.
    Unreachable {
        /// The article asked for.
        message_id: String,
        /// The last failure of each server that failed.
        errors: Vec<NntpError>,
    },
}

/// Why a connection could not be opened.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConnectFailure {
    /// The host name did not resolve, or the host did not accept a TCP connection.
    Tcp(IoFailure),
    /// The host is not a name or address that a TLS certificate can be checked against.
    HostName,
    /// The platform's certificate verifier could not be set up.
    Verifier(String),
    /// The TLS handshake failed.
    Tls(IoFailure),
    /// Connecting, the greeting and the login together took longer than allowed.
    TimedOut {
        /// The time allowed, in seconds.
        seconds: u64,
    },
}

/// How an open connection failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IoFailure {
    /// The server closed the connection.
    Closed,
    /// Nothing arrived, or nothing could be sent, for this long.
    TimedOut {
        /// The time allowed, in seconds.
        seconds: u64,
    },
    /// The operating system reported an error.
    Os {
        /// The kind of error.
        kind: io::ErrorKind,
        /// The error's message.
        message: String,
    },
}

impl From<io::Error> for IoFailure {
    fn from(error: io::Error) -> Self {
        Self::Os { kind: error.kind(), message: error.to_string() }
    }
}

impl NntpError {
    /// True when no server has the article, as opposed to failing to answer.
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing { .. })
    }
}

impl fmt::Display for NntpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { message_id } => write!(
                f,
                "article <{message_id}> is on none of the servers, so it has expired or been taken down; \
                 another posting of the release, or a server with longer retention, may have it"
            ),
            Self::InvalidMessageId { message_id } => write!(
                f,
                "{:?} is not a message-ID a server can be asked for (empty, over 250 bytes, or holding spaces, \
                 control characters or angle brackets); the NZB listing it is damaged",
                message_id
            ),
            Self::NoServers { message_id } => {
                write!(f, "cannot fetch article <{message_id}>: no Usenet server is configured; add one")
            }
            Self::Connect { server, failure } => write!(f, "could not connect to {server}: {failure}"),
            Self::Refused { server, reply } => write!(
                f,
                "{server} turned the connection away ({reply}); the account may be at its connection limit, \
                 so lower the connections setting or close other clients, or the server may be down"
            ),
            Self::Login { server, reply } => write!(
                f,
                "{server} rejected the login ({reply}); check the username and password, and that the account is active"
            ),
            Self::Reply { server, command, reply } => write!(
                f,
                "{server} answered {command} with an unexpected reply ({reply}), so the connection was dropped; \
                 if this keeps happening, the server is misbehaving"
            ),
            Self::Io { server, failure } => {
                write!(f, "lost the connection to {server}: {failure}; the next request opens a new one")
            }
            Self::Protocol { server, violation } => write!(
                f,
                "{server} broke the NNTP protocol: {violation}, so the connection was dropped; \
                 if this keeps happening, the server is misbehaving"
            ),
            Self::Unreachable { message_id, errors } => {
                write!(f, "no server could send article <{message_id}>, though it may exist; try again later. ")?;
                for (i, error) in errors.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" | ")?;
                    }
                    write!(f, "{error}")?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for ConnectFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(failure) => {
                write!(f, "{failure}; check the host name and port, and that the network is up")
            }
            Self::HostName => f.write_str("the host is not a valid name or address for TLS; check it for typos"),
            Self::Verifier(reason) => {
                write!(f, "the system's TLS certificates could not be loaded ({reason}); check the certificate store")
            }
            Self::Tls(failure) => write!(
                f,
                "the TLS handshake failed ({failure}); check that the port takes TLS (usually 563) and that the \
                 system trusts the server's certificate"
            ),
            Self::TimedOut { seconds } => write!(
                f,
                "no greeting and login within {seconds}s; the server may be overloaded or at the account's \
                 connection limit"
            ),
        }
    }
}

impl fmt::Display for IoFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("the server closed the connection"),
            Self::TimedOut { seconds } => write!(f, "the server went silent for {seconds}s"),
            Self::Os { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for NntpError {}
impl std::error::Error for ConnectFailure {}
impl std::error::Error for IoFailure {}
