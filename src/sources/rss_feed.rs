use chrono::{DateTime, Utc};
use futures::future::{BoxFuture, join_all};
use tracing::warn;

use super::{Source, SourceError, SourceResult, get_text, section_of};
use crate::article::Article;
use crate::text::{squash_ws, strip_html, truncate_chars};

/// Several RSS 2.0 feeds published under one source name.
pub struct RssSource {
    pub name: String,
    pub urls: Vec<String>,
}

impl Source for RssSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn fetch<'a>(
        &'a self,
        http: &'a reqwest::Client,
        since: DateTime<Utc>,
    ) -> BoxFuture<'a, SourceResult<Vec<Article>>> {
        Box::pin(async move {
            let results = join_all(self.urls.iter().map(|url| async move {
                let body = get_text(http, url).await?;
                parse_rss(&body, &self.name)
            }))
            .await;

            // A source with several feeds fails only if every feed failed.
            let mut articles = Vec::new();
            let mut last_err = None;
            for (url, res) in self.urls.iter().zip(results) {
                match res {
                    Ok(items) => articles.extend(items),
                    Err(e) => {
                        warn!(source = %self.name, %url, error = %e, "feed failed");
                        last_err = Some(e);
                    }
                }
            }
            if articles.is_empty()
                && let Some(e) = last_err
            {
                return Err(e);
            }
            articles.retain(|a| a.published >= since);
            Ok(articles)
        })
    }
}

/// Parse an RSS 2.0 document. Items without a link, title or a parseable
/// `pubDate` are skipped.
pub fn parse_rss(xml: &str, source: &str) -> SourceResult<Vec<Article>> {
    let channel = rss::Channel::read_from(xml.as_bytes()).map_err(|e| SourceError::Parse {
        what: format!("{source} RSS"),
        detail: e.to_string(),
    })?;

    Ok(channel
        .items()
        .iter()
        .filter_map(|item| {
            let title = squash_ws(item.title()?);
            let url = item
                .link()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .or_else(|| item.guid().map(|g| g.value().trim()))?
                .to_string();
            let published = DateTime::parse_from_rfc2822(item.pub_date()?.trim())
                .ok()?
                .with_timezone(&Utc);
            let summary = item
                .description()
                .map(strip_html)
                .filter(|s| !s.is_empty())
                .map(|s| truncate_chars(&s, 800));
            // Feed category if present, otherwise the URL's first path segment
            // (El Comercio: /gijon/, /deportes/, …), which helps the LLM filter.
            let section = item
                .categories()
                .first()
                .map(|c| squash_ws(c.name()))
                .or_else(|| section_of(&url));
            (!title.is_empty()).then(|| Article {
                source: source.to_string(),
                title,
                summary,
                url,
                published,
                section,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_20minutos_and_trims_titles() {
        let xml = include_str!("../../tests/fixtures/20minutos_asturias.xml");
        let items = parse_rss(xml, "20minutos").unwrap();
        assert!(items.len() >= 20, "got {}", items.len());
        let first = &items[0];
        assert_eq!(
            first.title,
            "Igualdad confirma el crimen machista de Gijón y eleva a 45 las asesinadas en 2026"
        );
        assert!(first.url.starts_with("https://www.20minutos.es/asturias/"));
        assert_eq!(
            first.published,
            Utc.with_ymd_and_hms(2026, 11, 16, 17, 57, 13).unwrap()
        );
        assert!(
            items
                .iter()
                .all(|a| !a.title.contains('\n') && a.title == a.title.trim())
        );
    }

    #[test]
    fn parses_eldiario_cdata_and_strips_html_descriptions() {
        let xml = include_str!("../../tests/fixtures/eldiario_economia.xml");
        let items = parse_rss(xml, "elDiario.es").unwrap();
        assert!(!items.is_empty());
        let first = &items[0];
        assert!(first.title.starts_with("Costa se rinde ante los austeros"));
        assert_eq!(
            first.url,
            "https://www.eldiario.es/economia/costa-rinde-austeros-presenta-proyecto-presupuesto-europeo-recorte-10-2_1_13682378.html"
        );
        let summary = first.summary.as_deref().unwrap();
        assert!(summary.starts_with("El presidente del Consejo Europeo"));
        assert!(!summary.contains('<'));
    }

    #[test]
    fn parses_elcomercio() {
        let xml = include_str!("../../tests/fixtures/elcomercio_asturias.xml");
        let items = parse_rss(xml, "El Comercio").unwrap();
        assert!(items.len() >= 50);
        assert!(items[0].title.contains("Cogersa"));
        assert!(items[0].summary.is_some());
        assert_eq!(items[0].section.as_deref(), Some("asturias"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_rss("<html>WAF Forbidden</html>", "x").is_err());
    }
}
