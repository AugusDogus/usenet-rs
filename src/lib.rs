//! Reads Usenet releases as files. An NZB lists a release's articles; [`resolve()`] reads
//! a few of them to learn how the release is packed and produces a [`Manifest`] that maps
//! every article onto the files a user sees, so any byte range can be fetched on demand.
//!
//! - [`nntp`]: a pool of NNTP connections across servers in priority order, with TLS,
//!   pipelining, per-server limits, backoff and reports.
//! - [`article`]: fetching one article from whichever server has a copy, and decoding it.
//! - [`yenc`]: decoding article bodies, with SIMD on x86_64 and aarch64.
//! - [`nzb`]: parsing NZB documents into files and segments.
//! - [`rar`]: reading RAR4 and RAR5 volume headers to find where stored files sit.
//! - [`par2`]: checking files against PAR2 recovery sets, and rebuilding what is damaged or
//!   missing from their recovery blocks.
//! - [`mod@resolve`]: turning an NZB into a [`Manifest`].
//! - [`manifest`]: the resolved layout, ready to store and read again.
//!
//! The `fake` feature adds a loopback NNTP server for testing code that uses this crate.
#![warn(missing_docs)]

pub mod article;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod manifest;
pub mod nntp;
pub mod nzb;
pub mod par2;
pub mod rar;
pub mod resolve;
pub mod yenc;

pub use manifest::Manifest;
pub use nntp::{Pool, Server};
pub use nzb::Nzb;
pub use resolve::{ResolveError, resolve};
