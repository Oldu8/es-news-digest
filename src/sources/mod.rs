//! News sources. Every source implements [`Source`]; the pipeline holds them as
//! `Vec<Box<dyn Source>>` and fetches all of them concurrently, so one broken
//! site never stops the others.

mod rss_feed;
mod sitemap;
mod spanified;

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use thiserror::Error;

pub use rss_feed::{RssSource, parse_rss};
pub use sitemap::{SitemapSource, parse_sitemap};
pub use spanified::{SpanifiedSource, find_issue_url, parse_issue};

use crate::article::Article;
use crate::config::SourceConfig;

/// Errors a single source can produce. Each source returns its own `Result`,
/// so the pipeline can log a failure and keep going with the rest.
#[derive(Debug, Error)]
pub enum SourceError {
    #[error("HTTP request to {url} failed: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{url} answered HTTP {status}")]
    Status { url: String, status: u16 },
    #[error("cannot parse {what}: {detail}")]
    Parse { what: String, detail: String },
}

pub type SourceResult<T> = Result<T, SourceError>;

/// A news source.
///
/// `fetch` returns a boxed future instead of being an `async fn`: trait objects
/// (`dyn Source`) need a concrete, nameable return type, and `BoxFuture` is the
/// usual way to give them one without extra macros.
pub trait Source: Send + Sync {
    fn name(&self) -> &str;
    /// Fetch articles published at or after `since`.
    fn fetch<'a>(
        &'a self,
        http: &'a reqwest::Client,
        since: DateTime<Utc>,
    ) -> BoxFuture<'a, SourceResult<Vec<Article>>>;
}

/// Build source objects from the config entries.
pub fn from_config(cfgs: &[SourceConfig]) -> Vec<Box<dyn Source>> {
    cfgs.iter()
        .map(|c| -> Box<dyn Source> {
            match c {
                SourceConfig::Spanified { name, url } => Box::new(SpanifiedSource {
                    name: name.clone(),
                    url: url.clone(),
                }),
                SourceConfig::Rss { name, urls } => Box::new(RssSource {
                    name: name.clone(),
                    urls: urls.clone(),
                }),
                SourceConfig::Sitemap {
                    name,
                    url,
                    sections,
                } => Box::new(SitemapSource {
                    name: name.clone(),
                    url: url.clone(),
                    sections: sections.clone(),
                }),
            }
        })
        .collect()
}

/// GET a URL and return the body as text, mapping errors to [`SourceError`].
pub(crate) async fn get_text(http: &reqwest::Client, url: &str) -> SourceResult<String> {
    let http_err = |source| SourceError::Http {
        url: url.to_string(),
        source,
    };
    let resp = http.get(url).send().await.map_err(http_err)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(SourceError::Status {
            url: url.to_string(),
            status: status.as_u16(),
        });
    }
    resp.text().await.map_err(http_err)
}

/// First path segment of a URL: `https://www.lne.es/oviedo/2026/…` → `oviedo`.
pub(crate) fn section_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let seg = parsed.path_segments()?.next()?;
    (!seg.is_empty()).then(|| seg.to_string())
}
