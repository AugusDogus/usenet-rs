//! yEnc decoding throughput, over the sample articles and synthetic ones.
//!
//! Set `USENET_CORPUS` to a directory of article bodies to also time those.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use usenet::yenc::decode;

/// Encodes like common posters do; see `tests/data/yenc/generate.py`.
fn encode(data: &[u8], name: &str) -> Vec<u8> {
    let mut out = format!("=ybegin line=128 size={} name={name}\r\n", data.len()).into_bytes();
    let mut col = 0;
    for (i, &b) in data.iter().enumerate() {
        let c = b.wrapping_add(42);
        let edge = col == 0 || col >= 127 || i + 1 == data.len();
        if matches!(c, 0 | b'\n' | b'\r' | b'=') || (edge && matches!(c, b'\t' | b' ')) || (col == 0 && c == b'.') {
            out.extend([b'=', c.wrapping_add(64)]);
            col += 2;
        } else {
            out.push(c);
            col += 1;
        }
        if col >= 128 {
            out.extend(b"\r\n");
            col = 0;
        }
    }
    out.extend(format!("\r\n=yend size={} crc32={:08x}\r\n", data.len(), crc32fast::hash(data)).bytes());
    out
}

fn pseudo_random(len: usize, mut state: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn bench(c: &mut Criterion) {
    let mut bodies: Vec<(String, Vec<u8>)> = ["multipart-first.yenc", "escapes.yenc", "single.yenc"]
        .iter()
        .map(|name| {
            let path = format!("{}/tests/data/yenc/{name}", env!("CARGO_MANIFEST_DIR"));
            (name.to_string(), std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path}: {e}")))
        })
        .collect();
    bodies.push(("random-768k".into(), encode(&pseudo_random(768_000, 1), "random")));
    // Every byte needs an escape: the worst case for the data loop.
    bodies.push(("all-escapes-256k".into(), encode(&[0xd6; 256_000], "escapes")));
    let mut group = c.benchmark_group("yenc::decode");
    for (name, body) in &bodies {
        group.throughput(Throughput::Bytes(body.len() as u64));
        group.bench_function(name.as_str(), |b| b.iter(|| decode(black_box(body))));
    }
    if let Ok(dir) = std::env::var("USENET_CORPUS") {
        let mut corpus = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {dir}: {e}")).flatten() {
            corpus.extend(std::fs::read(entry.path()));
        }
        let total: usize = corpus.iter().map(Vec::len).sum();
        group.throughput(Throughput::Bytes(total as u64));
        group.bench_function("USENET_CORPUS", |b| {
            b.iter(|| corpus.iter().map(|body| decode(black_box(body)).is_ok()).filter(|&ok| ok).count())
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
