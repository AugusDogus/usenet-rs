# usenet

Reads Usenet releases as files. Give it an NZB and your servers, and it works out which
bytes of which articles make up each file in the release, so any byte range of any file
can be fetched on demand. That is what streaming a video straight from Usenet needs.

- **NNTP**: TLS with the platform's certificate checks, a connection pool across servers
  in priority order (backup servers only see articles the primaries lack), per-server
  limits, backoff, timeouts, and per-server reports.
- **yEnc**: decoding with length and CRC checks.
- **NZB**: parsing, with reposted segments merged and the document's SHA-1 kept as its ID.
- **RAR**: RAR4 and RAR5 volume headers, read without fetching file data, to find where
  each stored file's bytes sit across the volumes.
- **Manifests**: a release resolved into files and the article pieces behind them, ready
  to store and read again.

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

## License

MIT
