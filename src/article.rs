//! Fetching one segment: trying each posted copy, then decoding it.

use std::fmt;

use crate::nntp::{NntpError, Pool};
use crate::yenc::{self, Part, YencError};

/// Why a segment could not be fetched.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ArticleError {
    /// No server sent any copy: see [`NntpError`].
    Nntp(NntpError),
    /// A server sent the article, but it does not decode.
    Yenc {
        /// The copy that failed to decode.
        message_id: String,
        /// Why it failed.
        error: YencError,
    },
}

impl ArticleError {
    /// True when no server has any copy of the article.
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Nntp(e) if e.is_missing())
    }
}

impl fmt::Display for ArticleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nntp(e) => write!(f, "{e}"),
            Self::Yenc { message_id, error } => write!(
                f,
                "article <{message_id}> arrived damaged: {error}; the copy on the server is bad, so another \
                 posting of the release may be needed"
            ),
        }
    }
}

// Display already says what the inner error says, so there is no separate source.
impl std::error::Error for ArticleError {}

/// Fetches and decodes the first copy of an article that some server has. `ids` are the
/// message-IDs of the same segment's copies, tried in order; a copy is skipped only when no
/// server has it.
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

/// True when some server has some copy of the article, asking about the copies in order.
pub async fn exists(pool: &Pool, ids: &[String]) -> Result<bool, NntpError> {
    for id in ids {
        if pool.stat(id).await? {
            return Ok(true);
        }
    }
    Ok(false)
}
