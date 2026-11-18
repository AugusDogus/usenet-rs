//! PAR2 throughput over a 64 MiB file in 768,000-byte slices, the size of a typical
//! article: checking every slice, collecting recovery blocks from a volume, and repairing one
//! or four slices, which adds every other slice into each recovery block.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use usenet::par2::{RecoverySet, Repair, Scanner, Slice};

const SIZE: usize = 64 << 20;
const SLICE: usize = 768_000;

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
    let data = pseudo_random(SIZE, 7);
    let files = usenet::fake::par2("movie", &[("movie.mkv", &data)], SLICE, 4);
    let set = RecoverySet::parse(&files[0].1).unwrap();
    let volumes: Vec<u8> = files[1..].iter().flat_map(|(_, v)| v.iter().copied()).collect();
    let read = |s: Slice, buf: &mut [u8]| {
        buf.copy_from_slice(&data[s.offset as usize..][..buf.len()]);
        Ok(buf.len())
    };

    let mut g = c.benchmark_group("par2");
    g.sample_size(10);
    g.throughput(Throughput::Bytes(SIZE as u64));
    g.bench_function("verify", |b| b.iter(|| assert!(set.verify(read).unwrap().is_empty())));
    g.throughput(Throughput::Bytes(volumes.len() as u64));
    g.bench_function("scan", |b| {
        b.iter(|| {
            let mut scanner = Scanner::new(&set);
            for article in volumes.chunks(SLICE) {
                scanner.push(article);
            }
            assert_eq!(scanner.blocks().len(), 4);
        })
    });
    let mut scanner = Scanner::new(&set);
    scanner.push(&volumes);
    let blocks = scanner.into_blocks();
    for damaged in [1usize, 4] {
        let missing: Vec<u32> = (0..damaged as u32).map(|k| k * 20 + 3).collect();
        g.throughput(Throughput::Bytes(SIZE as u64));
        g.bench_function(format!("repair {damaged}"), |b| {
            b.iter(|| {
                let mut repair = Repair::new(&set, &missing, blocks[..damaged].to_vec()).unwrap();
                for index in 0..set.slice_count() {
                    if repair.is_waiting(index) {
                        let s = set.slice(index).unwrap();
                        let _ = repair.feed(index, &data[s.offset as usize..][..s.length as usize]).unwrap();
                    }
                }
                black_box(repair.finish().unwrap())
            })
        });
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
