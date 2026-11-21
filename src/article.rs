use chrono::{DateTime, Utc};
use serde::Serialize;

/// One news item as it comes out of any source, before the LLM sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Article {
    /// Human-readable source name from the config, e.g. "El Comercio".
    pub source: String,
    pub title: String,
    /// Lead / short description, if the source has one.
    pub summary: Option<String>,
    pub url: String,
    pub published: DateTime<Utc>,
    /// Section of the site (first path segment for sitemaps, category for Spanified).
    pub section: Option<String>,
}
