//! A server that answers pipelined commands with random replies, good and bad: every request
//! ends, in time, with the right body or an error, and nothing panics.

use std::time::Duration;

use futures_util::future::join_all;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use usenet::nntp::NntpError;
use usenet::{Pool, Server};

/// A small deterministic generator, so a failure replays.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The body every article holds: lines that start with dots, to exercise dot-stuffing.
fn expected(id: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for i in 0..(id.len() % 7 + 2) {
        body.extend_from_slice(format!(".{id} line {i}\r\n..\r\n").as_bytes());
    }
    body
}

fn stuffed(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in body.split_inclusive(|&b| b == b'\n') {
        if line.first() == Some(&b'.') {
            out.push(b'.');
        }
        out.extend_from_slice(line);
    }
    out.extend_from_slice(b".\r\n");
    out
}

/// What to send for one command, and whether to hang up after.
fn reply(rng: &mut XorShift, verb: &str, id: &str) -> (Vec<u8>, bool) {
    let pick = rng.next() % 100;
    let reply = match pick {
        0..60 if verb == "BODY" => {
            let mut r = format!("222 0 <{id}>\r\n").into_bytes();
            r.extend(stuffed(&expected(id)));
            r
        }
        0..60 => format!("223 0 <{id}>\r\n").into_bytes(),
        60..70 => b"430 no such article\r\n".to_vec(),
        70..74 => return (Vec::new(), true),
        74..78 => (0..rng.next() % 40).map(|_| rng.next() as u8).chain(*b"\r\n").collect(),
        78..82 => "22\u{20ac} what\r\n".as_bytes().to_vec(),
        // Garbage after a good reply, where the next reply should start.
        82..86 => format!("430 no\r\ngarbage {id}\r\n").into_bytes(),
        86..89 => vec![b'x'; 3000],
        // A body where a STAT reply belongs.
        89..93 if verb == "STAT" => format!("222 0 <{id}>\r\nbody\r\n.\r\n").into_bytes(),
        89..93 => b"423 no such article number\r\n".to_vec(),
        93..96 => return (format!("222 0 <{id}>\r\nhalf a bo").into_bytes(), true),
        96..98 => b"\r\n".to_vec(),
        _ => {
            let mut r = b"222 0 <big>\r\n".to_vec();
            r.extend(std::iter::repeat_n(b"0123456789abcdef0123456789abcdef\r\n".as_slice(), 520_000).flatten());
            r
        }
    };
    (reply, false)
}

async fn serve(listener: TcpListener, seed: u64) {
    for n in 0u64.. {
        let Ok((stream, _)) = listener.accept().await else { return };
        let mut rng = XorShift(seed ^ (n + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            if write.write_all(b"200 random\r\n").await.is_err() {
                return;
            }
            while let Ok(Some(line)) = lines.next_line().await {
                let words: Vec<&str> = line.split_whitespace().collect();
                let id = words.get(1).map(|w| w.trim_matches(['<', '>'])).unwrap_or_default();
                let (out, close) = match words.first().copied() {
                    Some("AUTHINFO") if words.get(1) == Some(&"USER") => (b"381 more\r\n".to_vec(), false),
                    Some("AUTHINFO") => (b"281 ok\r\n".to_vec(), false),
                    Some(verb @ ("BODY" | "STAT")) => reply(&mut rng, verb, id),
                    Some("QUIT") => return,
                    _ => (b"500 what\r\n".to_vec(), false),
                };
                if write.write_all(&out).await.is_err() || close {
                    return;
                }
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn random_replies_end_every_request_with_the_right_body_or_an_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve(listener, 0x00c0_ffee_d00d_f00d));
    let server = Server {
        host: "127.0.0.1".into(),
        port,
        tls: false,
        username: "u".into(),
        password: "p".into(),
        connections: 3,
        priority: 0,
        pipeline: Server::DEFAULT_PIPELINE,
    };
    let pool = Pool::new(vec![server]);
    let (mut bodies, mut errors) = (0, 0);
    for batch in 0..10 {
        let ids: Vec<String> = (0..40).map(|i| format!("{batch}.{i}@random.invalid")).collect();
        let requests = ids.iter().enumerate().map(|(i, id)| {
            let pool = pool.clone();
            async move { if i % 4 == 3 { pool.stat(id).await.map(|_| None) } else { pool.body(id).await.map(Some) } }
        });
        let results =
            tokio::time::timeout(Duration::from_secs(60), join_all(requests)).await.expect("no request hangs");
        for (id, result) in ids.iter().zip(results) {
            match result {
                Ok(Some(body)) => {
                    assert_eq!(body, expected(id), "{id}");
                    bodies += 1;
                }
                Ok(None) => {}
                Err(NntpError::Missing { .. } | NntpError::Unreachable { .. }) => errors += 1,
                Err(e) => panic!("the pool ends in Missing or Unreachable, not {e:?}"),
            }
        }
    }
    assert!(bodies > 50 && errors > 10, "{bodies} bodies, {errors} errors");
    let report = &pool.report()[0];
    assert!(report.failed > 0 && report.answered > 0, "{report:?}");
}
