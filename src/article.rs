//! Fetching one segment: trying each posted copy, then decoding it.

use std::fmt;

use crate::nntp::{NntpError, Pool};
use crate::yenc::{self, Part, YencError};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ArticleError {
    Nntp(NntpError),
    Yenc { message_id: String, error: YencError },
}

impl ArticleError {
    /// True when no server has any copy of the article.
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Nntp(NntpError::Missing { .. }))
    }
}

impl fmt::Display for ArticleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nntp(e) => write!(f, "{e}"),
            Self::Yenc { message_id, error } => write!(f, "article <{message_id}>: {error}"),
        }
    }
}

impl std::error::Error for ArticleError {}

/// Fetches and decodes the first copy of an article that some server has.
pub async fn fetch(pool: &Pool, ids: &[String]) -> Result<Part, ArticleError> {
    let mut last = None;
    for id in ids {
        match pool.body(id).await {
            Ok(body) => {
                return yenc::decode(&body).map_err(|error| ArticleError::Yenc { message_id: id.clone(), error });
            }
            Err(e @ NntpError::Missing { .. }) => last = Some(e),
            Err(e) => return Err(ArticleError::Nntp(e)),
        }
    }
    let message_id = ids.first().cloned().unwrap_or_default();
    Err(ArticleError::Nntp(last.unwrap_or(NntpError::Missing { message_id })))
}

/// True when some server has some copy of the article.
pub async fn exists(pool: &Pool, ids: &[String]) -> Result<bool, NntpError> {
    for id in ids {
        if pool.stat(id).await? {
            return Ok(true);
        }
    }
    Ok(false)
}
