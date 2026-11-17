# usenet

Reads Usenet releases as files. Give it an NZB and your servers, and it works out which
bytes of which articles make up each file in the release, so any byte range of any file
can be fetched on demand. That is what streaming a video straight from Usenet needs.

- **NNTP**: TLS with the platform's certificate checks, and a connection pool across
  servers in priority order (backup servers only see articles the primaries lack), with
  pipelined requests, per-server limits, backoff, timeouts, and per-server reports.
- **yEnc**: decoding with SSE2, AVX2 or NEON (about 3 GB/s on one core of a Ryzen 9 5950X),
  with length, truncation and CRC checks.
- **NZB**: parsing, with reposted segments merged. A release is named by its segments, so
  the decoy metadata indexers vary on every download does not change its ID.
- **RAR**: RAR4 and RAR5 volume headers, checksums verified, read without fetching file
  data, to find where each stored file's bytes sit across the volumes.
- **Manifests**: a release resolved into files and the article pieces behind them, ready
  to store and read again. Seeking anywhere costs one article.

Every error is a type you can match on, and its message says what happened and what to do.
The parsers are fuzzed, and none of them panics on any input.

It does not repair (PAR2) or decompress. Releases packed in compressed or encrypted RAR
archives are reported as such. Pair it with a PAR2 or RAR crate if you need those.

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

`examples/resolve.rs` resolves NZBs against real servers and prints what it finds.

## Testing

The `fake` feature adds a loopback NNTP server and a way to post releases to it, for
testing code that uses this crate without a Usenet account.

The crate's own tests run against real NZBs of free films, archives written by `rar`, and
seeded mutations of both. `cargo bench` measures each module; `fuzz/` holds cargo-fuzz
targets for the yEnc, NZB, NNTP reply, RAR and manifest parsers.

## License

MIT
