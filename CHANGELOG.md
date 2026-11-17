# Changelog

## 0.2.0

Breaking changes:

- `Server` has a `pipeline` field: how many requests one connection carries at once.
  `Server::DEFAULT_PIPELINE` is 4; 1 turns pipelining off. `Pool::capacity` counts
  connections times depth.
- Errors are typed throughout: `NntpError` (with `ConnectFailure` and `IoFailure`),
  `ArticleError`, `YencError`, `NzbError`, `RarError` with `ScanError` and `Defect`,
  `ResolveError` with `LayoutError`, and `ManifestError` with `Tiling`.
- `Nzb::sha1` is now `Nzb::id`, a hash of the segments rather than the document, so an NZB
  an indexer randomizes per download keeps one ID. `Manifest::nzb_sha1` is now
  `Manifest::release_id`; the stored form is unchanged.
- `Manifest` has no `format` field; the version is written and checked on the wire.
  `Manifest::from_bytes` rejects unsafe and duplicate paths.
- `rar::scan` returns `ScanError`. `nntp::Connection` is no longer public.
- `resolve` turns `:` and NUL in file names into `_`, and an NZB with no files is an error.

Added:

- Pipelined BODY and STAT on each connection, about 2 to 4 times the throughput when the
  round trip dominates.
- SIMD yEnc decoding (SSE2, AVX2, NEON), about 4 times faster on real articles.
- RAR4 and RAR5 header checksums are verified.
- Message-IDs are validated before they are sent.
- TCP_NODELAY on connections; QUIT when the last `Pool` is dropped.
- Criterion benches and cargo-fuzz targets.

Fixed:

- A canceled request no longer leaks a connection slot.
- A yEnc article cut before its trailer is reported as truncated.
- NZB parsing is about 25% faster.

## 0.1.0

First release.
