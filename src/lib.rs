//! Reads Usenet releases as files. An NZB lists a release's articles; [`resolve`] reads
//! a few of them to learn how the release is packed and produces a [`Manifest`] that maps
//! every article onto the files a user sees, so any byte range can be fetched on demand.

pub mod article;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod manifest;
pub mod nntp;
pub mod nzb;
pub mod rar;
pub mod resolve;
pub mod yenc;

pub use manifest::Manifest;
pub use nntp::{Pool, Server};
pub use nzb::Nzb;
pub use resolve::{ResolveError, resolve};
