//! RAR header scans per volume, and resolving a many-volume stored release on the loopback
//! server, with the article fetches a resolve costs.
//!
//! cargo bench --features fake --bench rar

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use usenet::fake::{FakeServer, Posting};
use usenet::{Nzb, Pool, rar, resolve};

/// Runs a future whose every await is ready, as a scan over in-memory reads is.
fn ready<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(out) => out,
        Poll::Pending => panic!("the future awaited something that was not ready"),
    }
}

fn volume(path: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/rar").join(path);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("scan");
    // (case, bytes handed over as the first article). A short head makes the late header a read.
    let cases = [
        ("rar4/volumes_new/arc.part1.rar", usize::MAX),
        ("rar5/volumes_new/arc.part1.rar", usize::MAX),
        ("rar4/multi_dirs_unicode/arc.rar", usize::MAX),
        ("rar5/quickopen/arc.part3.rar", usize::MAX),
        ("rar5/late_header/arc.rar", 1_500),
    ];
    for (case, head) in cases {
        let bytes = volume(case);
        let head = &bytes[..bytes.len().min(head)];
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::from_parameter(case), &bytes, |b, bytes| {
            b.iter(|| {
                let read = |at: u64, len: usize| {
                    let at = at as usize;
                    std::future::ready(Ok::<_, ()>(bytes[at..bytes.len().min(at + len)].to_vec()))
                };
                ready(rar::scan(head.to_vec(), bytes.len() as u64, read)).expect("the fixture scans")
            });
        });
    }
    group.finish();
}

fn resolve_release(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let mut group = c.benchmark_group("resolve");
    group.sample_size(20);
    for volumes in [10usize, 80] {
        // 50 KB of the film per volume, in 10 KB articles.
        let film: Vec<u8> = (0..volumes * 50_000).map(|i| (i * 31 % 251) as u8).collect();
        let posting = Posting::stored_rar("Film.2024", "Film.2024.mkv", &film, 50_000, 10_000);
        let server = FakeServer::start();
        server.add(&posting);
        let pool = Pool::new(vec![server.config(8, 0)]);
        let nzb = Nzb::parse(&posting.nzb).unwrap();
        let articles = posting.ids().count();
        let before = server.bodies();
        let manifest = runtime.block_on(resolve(&nzb, &pool)).expect("the release resolves");
        let fetched = server.bodies() - before;
        assert_eq!(manifest.files.len(), 2);
        // One article per volume: every header sits in its volume's first article.
        assert_eq!(fetched, volumes as u64, "fetches for {volumes} volumes");
        eprintln!("resolve/{volumes} volumes: {fetched} of {articles} articles fetched");
        group.throughput(Throughput::Elements(volumes as u64));
        group.bench_function(BenchmarkId::new("volumes", volumes), |b| {
            b.iter(|| runtime.block_on(resolve(&nzb, &pool)).expect("the release resolves"));
        });
    }
    group.finish();
}

criterion_group!(benches, scan, resolve_release);
criterion_main!(benches);
