//! How long a seek waits while a download keeps the pool busy, and what the download gets.
//!
//! Each scenario runs a loopback server with a link speed, a per-connection speed and a
//! latency, and a download of `Pool::capacity` background workers, as newsreel runs one. After
//! a warm-up, a seek fetches one urgent article every two seconds, then four at once (a seek
//! and its read-ahead). It prints the seeks' latency, the download's throughput, and the
//! link's: the download's and the seeks' together.
//!
//! ```text
//! cargo run --release --features fake --example priority [scenario...]
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use usenet::Pool;
use usenet::fake::{FakeServer, Posting};
use usenet::nntp::Priority;

const MIB: u64 = 1 << 20;
/// A typical article's decoded size.
const ARTICLE: usize = 768_000;
const WARM_UP: Duration = Duration::from_secs(15);
const MEASURE: Duration = Duration::from_secs(45);

struct Scenario {
    name: &'static str,
    /// Bytes per second across all connections; 0 is unlimited.
    link: u64,
    /// Bytes per second on each connection; 0 is unlimited.
    connection: u64,
    latency: Duration,
    connections: u16,
    pipeline: u8,
}

const SCENARIOS: [Scenario; 4] = [
    Scenario {
        name: "slow",
        link: 12 * MIB,
        connection: 0,
        latency: Duration::from_millis(50),
        connections: 100,
        pipeline: 4,
    },
    Scenario {
        name: "slow-capped",
        link: 12 * MIB,
        connection: MIB,
        latency: Duration::from_millis(50),
        connections: 100,
        pipeline: 4,
    },
    Scenario {
        name: "fast",
        link: 200 * MIB,
        connection: 4 * MIB,
        latency: Duration::from_millis(50),
        connections: 100,
        pipeline: 4,
    },
    Scenario {
        name: "few-far",
        link: 40 * MIB,
        connection: 4 * MIB,
        latency: Duration::from_millis(100),
        connections: 8,
        pipeline: 4,
    },
];

/// The handle seeks use.
fn urgent(pool: &Pool) -> Pool {
    pool.with_priority(Priority::Urgent)
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

async fn run(s: &Scenario) {
    let server = FakeServer::start();
    server.set_rate(s.link);
    server.set_connection_rate(s.connection);
    server.set_latency(s.latency);
    let data: Vec<u8> = (0..64 * ARTICLE).map(|i| (i * 7 % 251) as u8).collect();
    let posting = Posting::new(s.name, &[("a.bin", &data)], ARTICLE);
    server.add(&posting);
    let ids: Arc<Vec<String>> = Arc::new(posting.ids().map(String::from).collect());
    let mut config = server.config(s.connections, 0);
    config.pipeline = s.pipeline.try_into().expect("a pipeline of at least 1");
    let pool = Pool::new(vec![config]);

    let (stop, bytes, next) =
        (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let mut workers = Vec::new();
    for _ in 0..pool.capacity() {
        let (pool, ids, stop, bytes, next) = (pool.clone(), ids.clone(), stop.clone(), bytes.clone(), next.clone());
        workers.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let k = next.fetch_add(1, Ordering::Relaxed) as usize % ids.len();
                if let Ok(body) = pool.body(&ids[k]).await {
                    bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                }
            }
        }));
    }
    tokio::time::sleep(WARM_UP).await;
    let (start, before) = (Instant::now(), bytes.load(Ordering::Relaxed));
    let seek = urgent(&pool);
    let (mut single, mut burst) = (Vec::new(), Vec::new());
    let urgent_bytes = AtomicU64::new(0);
    let fetch = async |k: usize| {
        let body = seek.body(&ids[k % ids.len()]).await.expect("an urgent article");
        urgent_bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
    };
    let mut k = 0;
    while start.elapsed() < MEASURE {
        tokio::time::sleep(Duration::from_secs(2)).await;
        k += 1;
        let t = Instant::now();
        fetch(k).await;
        single.push(t.elapsed());
        tokio::time::sleep(Duration::from_secs(2)).await;
        let t = Instant::now();
        futures_util::future::join_all((0..4).map(|j| fetch(k * 4 + j))).await;
        burst.push(t.elapsed());
    }
    let elapsed = start.elapsed().as_secs_f64();
    let limit = pool.stats().background_limit;
    let background = (bytes.load(Ordering::Relaxed) - before) as f64 / elapsed / MIB as f64;
    let total = background + urgent_bytes.load(Ordering::Relaxed) as f64 / elapsed / MIB as f64;
    stop.store(true, Ordering::Relaxed);
    for w in workers {
        let _ = w.await;
    }
    single.sort();
    burst.sort();
    let ms = |d: Duration| d.as_millis();
    println!(
        "{:<12} seek p50 {:>6} ms p90 {:>6} ms max {:>6} ms | seek+3 p50 {:>6} ms p90 {:>6} ms | download {:>5.1} MiB/s, all {:>5.1} MiB/s | limit {} ({} seeks)",
        s.name,
        ms(percentile(&single, 0.5)),
        ms(percentile(&single, 0.9)),
        ms(percentile(&single, 1.0)),
        ms(percentile(&burst, 0.5)),
        ms(percentile(&burst, 0.9)),
        background,
        total,
        limit,
        single.len(),
    );
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let chosen: Vec<String> = std::env::args().skip(1).collect();
    for s in SCENARIOS.iter().filter(|s| chosen.is_empty() || chosen.iter().any(|c| c == s.name)) {
        run(s).await;
    }
}
