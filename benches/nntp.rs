//! The pool against the loopback fake server: one article at a time, a burst on one
//! connection at each pipeline depth with a distant server's latency, and many requests at
//! once on a few connections.

use std::hint::black_box;
use std::num::NonZeroU8;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures_util::future::join_all;
use tokio::runtime::Runtime;
use usenet::fake::{FakeServer, Posting};
use usenet::{Pool, Server};

/// About the size of a real article.
const PART: usize = 768 * 1024;

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("a runtime starts")
}

fn posting(parts: usize) -> Posting {
    let data: Vec<u8> = (0..PART * parts).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    Posting::new("bench", &[("bench.bin", &data)], PART)
}

fn pool(server: &FakeServer, connections: u16, depth: u8) -> Pool {
    let pipeline = NonZeroU8::new(depth).unwrap_or(NonZeroU8::MIN);
    Pool::new(vec![Server { pipeline, ..server.config(connections, 0) }])
}

fn single(c: &mut Criterion) {
    let rt = runtime();
    let server = FakeServer::start();
    let posting = posting(4);
    server.add(&posting);
    let ids: Vec<String> = posting.ids().map(str::to_string).collect();
    let pool = pool(&server, 1, 4);
    rt.block_on(pool.body(&ids[0])).expect("the fake serves its posting");
    let mut group = c.benchmark_group("single");
    group.throughput(Throughput::Bytes(PART as u64));
    group.bench_function("body", |b| b.iter(|| rt.block_on(pool.body(black_box(&ids[1]))).map(|body| body.len())));
    group.bench_function("stat", |b| b.iter(|| rt.block_on(pool.stat(black_box(&ids[1])))));
    group.finish();
}

/// 16 articles at once on one connection, 10 ms from the server.
fn depth(c: &mut Criterion) {
    let rt = runtime();
    let server = FakeServer::start();
    let posting = posting(16);
    server.add(&posting);
    server.set_latency(Duration::from_millis(10));
    let ids: Vec<String> = posting.ids().map(str::to_string).collect();
    let mut group = c.benchmark_group("one_connection_10ms");
    group.sample_size(10).throughput(Throughput::Bytes((PART * ids.len()) as u64));
    for depth in [1u8, 2, 4, 8] {
        let pool = pool(&server, 1, depth);
        rt.block_on(pool.stat(&ids[0])).expect("the fake answers");
        group.bench_with_input(BenchmarkId::new("depth", depth), &pool, |b, pool| {
            b.iter(|| rt.block_on(join_all(ids.iter().map(|id| pool.body(id)))).iter().filter(|r| r.is_ok()).count())
        });
    }
    group.finish();
}

/// 256 STATs at once on four connections, 5 ms from the server.
fn concurrent(c: &mut Criterion) {
    let rt = runtime();
    let server = FakeServer::start();
    let posting = posting(1);
    server.add(&posting);
    server.set_latency(Duration::from_millis(5));
    let id = posting.ids().next().map(str::to_string).unwrap_or_default();
    let mut group = c.benchmark_group("concurrent_stat_5ms");
    group.sample_size(10).throughput(Throughput::Elements(256));
    for depth in [1u8, 4, 8] {
        let pool = pool(&server, 4, depth);
        rt.block_on(join_all((0..4).map(|_| pool.stat(&id))));
        group.bench_with_input(BenchmarkId::new("depth", depth), &pool, |b, pool| {
            b.iter(|| rt.block_on(join_all((0..256).map(|_| pool.stat(&id)))).len())
        });
    }
    group.finish();
}

criterion_group!(benches, single, depth, concurrent);
criterion_main!(benches);
