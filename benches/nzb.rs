//! NZB parsing throughput, over the sample NZBs and a synthetic large one.
//!
//! Set `USENET_CORPUS` to a directory of NZBs to also time the largest of them.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use usenet::Nzb;

/// An NZB of `files` files with `segments` segments each, shaped like an indexer's.
fn synthetic(files: usize, segments: usize) -> Vec<u8> {
    let mut doc = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n\
         <head>\n <meta type=\"name\">Synthetic.Release</meta>\n</head>\n",
    );
    for f in 1..=files {
        doc += &format!(
            "<file poster=\"poster@example.com\" date=\"1700000000\" \
             subject=\"[{f}/{files}] - &quot;synthetic.part{f:03}.rar&quot; yEnc (1/{segments})\">\n\
             <groups>\n<group>alt.binaries.test</group>\n</groups>\n<segments>\n"
        );
        for s in 1..=segments {
            doc += &format!(
                "<segment bytes=\"739{s:03}\" number=\"{s}\">Xk3PqL8vR2mN9tYbWc4Hs7Gd{f:04}{s:04}@example.com</segment>\n"
            );
        }
        doc += "</segments>\n</file>\n";
    }
    doc += "</nzb>\n";
    doc.into_bytes()
}

fn bench(c: &mut Criterion) {
    let mut docs: Vec<(String, Vec<u8>)> =
        ["Big.Buck.Bunny.2008.1080p.x264-60fps.Surround.nzb", "BigBuckBunny.640x360.m4v.nzb"]
            .iter()
            .map(|name| {
                let path = format!("{}/tests/data/nzb/{name}", env!("CARGO_MANIFEST_DIR"));
                (name.to_string(), std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path}: {e}")))
            })
            .collect();
    docs.push(("synthetic-100x1000".into(), synthetic(100, 1000)));
    if let Ok(dir) = std::env::var("USENET_CORPUS") {
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {dir}: {e}"));
        let biggest = entries.flatten().filter_map(|e| std::fs::read(e.path()).ok()).max_by_key(Vec::len);
        docs.extend(biggest.map(|doc| ("USENET_CORPUS largest".to_string(), doc)));
    }
    let mut group = c.benchmark_group("Nzb::parse");
    group.sample_size(20);
    for (name, doc) in &docs {
        group.throughput(Throughput::Bytes(doc.len() as u64));
        group.bench_function(name.as_str(), |b| b.iter(|| Nzb::parse(black_box(doc))));
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
