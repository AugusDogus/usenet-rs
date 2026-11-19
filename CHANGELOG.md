# Changelog

## 0.3.0 (unreleased)

Breaking changes:

- The pool holds background requests to a limit it tunes as it runs: the fewest in flight
  that keep throughput within 3% of the best it has measured lately. A download that starts
  `Pool::capacity` requests now has only that many in flight; the rest wait their turn.
  `PoolStats` has a `background_limit` field that reports it.

Added:

- Request priority. `Pool::with_priority(Priority::Urgent)` makes a handle on the same
  connections whose requests skip the background limit and take freed slots first, for a
  seek or a release's first reads. On a loopback server limited to 12 MiB/s with 100
  connections, pipeline 4 and a download running, a seek's article took 27 s before and
  about 0.35 s now (1.2 s instead of 27 s when each connection is also capped at 1 MiB/s),
  with the link within 2.5% as full as before (`examples/priority.rs`). An urgent request
  never queues behind a background one on a connection, since replies come in order: with
  every connection carrying background requests, it waits for the first to free up.
- `par2`: PAR2 2.0 recovery sets. `RecoverySet` parses an index or volume and checks slices
  against it; `slices_in` maps a missing byte range to slices; `Scanner` collects recovery
  blocks from volumes as their articles arrive; `Repair` rebuilds damaged slices by
  Reed-Solomon over GF(2^16), with SSSE3, AVX2 or NEON, as every other slice of the set is
  fed to it; `RecoverySet::repair` does the same through a reader and a writer;
  `Par2Files` finds an NZB's PAR2 files, named `name.vol7+8.par2` or `name.vol-07.par2`,
  and orders the volumes to fetch for enough blocks. `RecoverySet::file_starting` matches a
  posted file to the set by its size and first article, since posters rename files.
  `examples/repair.rs` repairs a real release this way.
- A yEnc length, CRC or truncation error no longer ends with "so fetch another copy": the
  caller knows whether another copy exists, and `ArticleError` says what follows.
- `Manifest::par2` lists the posted files whose names hide that they are PAR2 files, as the
  resolver recognized them by their first article, with each one's recovery set, and
  `Par2Files::find_resolved` finds their sets beside those found by name. A manifest without
  any writes no `par2` field, so manifests stay readable by 0.2.
- The `fake` server can cap each connection's speed (`FakeServer::set_connection_rate`), serve
  an article cut short before its trailer (`FakeServer::damage`), `fake::par2` writes PAR2
  files to post with a release, and `fake::rar_volumes` gives the volumes `Posting::stored_rar`
  posts, to protect with them.
- Criterion benches for PAR2 checking, scanning and repair, and a cargo-fuzz target for it.

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
