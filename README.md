# usenet

Reads Usenet releases as files. Give it an NZB and your servers, and it works out which
bytes of which articles make up each file in the release, so any byte range of any file
can be fetched on demand. That is what streaming a video straight from Usenet needs.

- **NNTP**: TLS with the platform's certificate checks, and a connection pool across
  servers in priority order (backup servers only see articles the primaries lack), with
  pipelined requests, per-server limits, backoff, timeouts, and per-server reports.
  Requests are urgent (a seek) or background (a download); the pool holds background
  requests to the fewest that keep the link full, so a seek is not stuck behind them.
- **yEnc**: decoding with SSE2, AVX2 or NEON (about 3 GB/s on one core of a Ryzen 9 5950X),
  with length, truncation and CRC checks.
- **NZB**: parsing, with reposted segments merged. A release is named by its segments, so
  the decoy metadata indexers vary on every download does not change its ID.
- **RAR**: RAR4 and RAR5 volume headers, checksums verified, read without fetching file
  data, to find where each stored file's bytes sit across the volumes.
- **Manifests**: a release resolved into files and the article pieces behind them, ready
  to store and read again. Seeking anywhere costs one article.
- **PAR2**: recovery sets parsed and checked, and damaged or missing slices rebuilt from
  recovery blocks (Reed-Solomon over GF(2^16) with SSSE3, AVX2 or NEON), for an article
  that no server has. Recovery blocks are collected from volumes as their articles arrive.

Every error is a type you can match on, and its message says what happened and what to do.
The parsers are fuzzed, and none of them panics on any input.

It does not decompress. Releases packed in compressed or encrypted RAR archives are
reported as such. Pair it with a RAR crate if you need those.

## Example

```rust
use usenet::{Nzb, Pool, Server, article, resolve};

let pool = Pool::new(vec![Server {
    host: "news.example.com".into(),
    port: 563,
    tls: true,
    username: "user".into(),
    password: "secret".into(),
    connections: 8,
    priority: 0,
    pipeline: Server::DEFAULT_PIPELINE,
}]);
let nzb = Nzb::parse(&std::fs::read("release.nzb")?)?;
let manifest = resolve(&nzb, &pool).await?;

// Read the first file's first piece.
let file = &manifest.files[0];
let piece = &file.pieces[0];
let part = article::fetch(&pool, &piece.ids).await?;
let bytes = &part.data[piece.skip as usize..(piece.skip + piece.length) as usize];
```

A seek goes through an urgent handle on the same connections:

```rust
use usenet::nntp::Priority;

let seek = pool.with_priority(Priority::Urgent);
let part = article::fetch(&seek, &piece.ids).await?;
```

When an article is gone from every server, the release's PAR2 files can rebuild it:

```rust
use usenet::par2::{Par2Files, RecoverySet, Scanner};

let par2 = &Par2Files::find(&nzb)[0];
let set = RecoverySet::parse(&fetch_file(par2.index).await?)?;
let file = set.file_named("movie.part01.rar").unwrap();
let damaged: Vec<u32> = set.slices_in(file, begin..end).collect();
let mut scanner = Scanner::new(&set);
for volume in par2.choose(damaged.len() as u32, set.slice_size()) {
    if scanner.blocks().len() >= damaged.len() {
        break;
    }
    scanner.push(&fetch_file(volume).await?);
}
// Reads every other slice of the set, so the rest of the release must be on disk.
set.repair(&damaged, scanner.into_blocks(), read_slice, write_slice)?;
```

`examples/resolve.rs` resolves NZBs against real servers and prints what it finds.
`examples/priority.rs` measures how long a seek waits while a download runs, and
`examples/repair.rs` damages a downloaded release and repairs it from its PAR2 volumes.

## Testing

The `fake` feature adds a loopback NNTP server, with link and per-connection speed limits
and latency, a way to post releases to it, and PAR2 files to post with them, for testing
code that uses this crate without a Usenet account.

The crate's own tests run against real NZBs of free films, archives written by `rar`, a
recovery set written by par2cmdline, and seeded mutations of them. `cargo bench` measures
each module; `fuzz/` holds cargo-fuzz targets for the yEnc, NZB, NNTP reply, RAR, manifest
and PAR2 parsers.

## Releasing

Bump `version` in `Cargo.toml`, add the changes to `CHANGELOG.md`, and push a matching tag
(`git tag v0.3.0 && git push origin v0.3.0`). The release workflow runs CI and publishes to
crates.io with the `CARGO_REGISTRY_TOKEN` repository secret.

## License

MIT
