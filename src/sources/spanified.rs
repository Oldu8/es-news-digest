use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Europe::Madrid;
use futures::future::BoxFuture;
use scraper::{Html, Selector};

use super::{Source, SourceError, SourceResult, get_text};
use crate::article::Article;
use crate::text::squash_ws;

/// Spanified "Spain Daily": an English daily briefing with 7–12 stories,
/// each linking to the original Spanish article.
pub struct SpanifiedSource {
    pub name: String,
    pub url: String,
}

impl Source for SpanifiedSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn fetch<'a>(
        &'a self,
        http: &'a reqwest::Client,
        since: DateTime<Utc>,
    ) -> BoxFuture<'a, SourceResult<Vec<Article>>> {
        Box::pin(async move {
            let home = get_text(http, &self.url).await?;
            let issue_url = find_issue_url(&home, &self.url)?;
            let issue = get_text(http, &issue_url).await?;
            let mut items = parse_issue(&issue, &issue_url, &self.name)?;
            items.retain(|a| a.published >= since);
            Ok(items)
        })
    }
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("static selector is valid")
}

fn parse_err(detail: impl Into<String>) -> SourceError {
    SourceError::Parse {
        what: "Spanified".into(),
        detail: detail.into(),
    }
}

/// Find the latest `/archive/YYYY-MM-DD` link on the home page.
pub fn find_issue_url(home_html: &str, base: &str) -> SourceResult<String> {
    let doc = Html::parse_document(home_html);
    let base = url::Url::parse(base).map_err(|e| parse_err(e.to_string()))?;
    let latest = doc
        .select(&sel("a[href]"))
        .filter_map(|a| a.value().attr("href"))
        .filter_map(|href| {
            let date = href.trim_end_matches('/').rsplit('/').next()?;
            href.contains("/archive/")
                .then(|| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
                .flatten()
                .map(|d| (d, href))
        })
        .max_by_key(|(d, _)| *d)
        .ok_or_else(|| parse_err("no /archive/YYYY-MM-DD link on the home page"))?;
    base.join(latest.1)
        .map(|u| u.to_string())
        .map_err(|e| parse_err(e.to_string()))
}

/// When the issue was published: `<time datetime>` if present, otherwise
/// 08:00 Madrid on the date in the URL.
fn issue_time(doc: &Html, issue_url: &str) -> SourceResult<DateTime<Utc>> {
    if let Some(dt) = doc
        .select(&sel("time[datetime]"))
        .filter_map(|t| t.value().attr("datetime"))
        .find_map(|v| DateTime::parse_from_rfc3339(v).ok())
    {
        return Ok(dt.with_timezone(&Utc));
    }
    let date = issue_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .ok_or_else(|| parse_err(format!("no date in {issue_url}")))?;
    Madrid
        .from_local_datetime(&date.and_hms_opt(8, 0, 0).expect("valid time"))
        .earliest()
        .map(|d| d.with_timezone(&Utc))
        .ok_or_else(|| parse_err("invalid local time"))
}

/// Parse one daily issue page into articles. The article URL is the external
/// link to the original source; the source name becomes "Spain Daily / El País".
pub fn parse_issue(html: &str, issue_url: &str, source: &str) -> SourceResult<Vec<Article>> {
    let doc = Html::parse_document(html);
    let published = issue_time(&doc, issue_url)?;
    let (s_title, s_body, s_src, s_cat) = (
        sel("h2.story-title"),
        sel("p.story-body"),
        sel(".story-source a[href]"),
        sel(".story-category"),
    );

    let articles: Vec<Article> = doc
        .select(&sel("article.story"))
        .filter_map(|story| {
            let title = squash_ws(&story.select(&s_title).next()?.text().collect::<String>());
            let summary = story
                .select(&s_body)
                .next()
                .map(|b| squash_ws(&b.text().collect::<String>()));
            let link = story.select(&s_src).find(|a| {
                a.value()
                    .attr("href")
                    .is_some_and(|h| h.starts_with("http"))
            })?;
            let outlet = squash_ws(&link.text().collect::<String>());
            let section = story
                .select(&s_cat)
                .next()
                .map(|c| squash_ws(&c.text().collect::<String>()));
            Some(Article {
                source: if outlet.is_empty() {
                    source.to_string()
                } else {
                    format!("{source} / {outlet}")
                },
                title,
                summary,
                url: link.value().attr("href")?.to_string(),
                published,
                section,
            })
        })
        .collect();

    if articles.is_empty() {
        return Err(parse_err("no article.story blocks (layout changed?)"));
    }
    Ok(articles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_latest_issue_on_home_page() {
        let home = include_str!("../../tests/fixtures/spanified_home.html");
        assert_eq!(
            find_issue_url(home, "https://spanified.com").unwrap(),
            "https://spanified.com/archive/2026-11-21"
        );
    }

    #[test]
    fn parses_issue_stories() {
        let html = include_str!("../../tests/fixtures/spanified_issue.html");
        let items = parse_issue(
            html,
            "https://spanified.com/archive/2026-11-21",
            "Spain Daily",
        )
        .unwrap();
        assert!((7..=15).contains(&items.len()), "got {}", items.len());
        let a = &items[0];
        assert_eq!(
            a.title,
            "Madrid–Andalucía rail repairs could take two weeks"
        );
        assert_eq!(a.source, "Spain Daily / El País");
        assert!(a.url.starts_with("https://elpais.com/economia/2026-11-20/"));
        assert!(
            a.summary
                .as_deref()
                .unwrap()
                .starts_with("The Transport Ministry")
        );
        assert_eq!(a.section.as_deref(), Some("💰 Economy"));
        assert_eq!(a.published.to_rfc3339(), "2026-11-21T07:00:00+00:00");
        // No relative links to spanified itself.
        assert!(items.iter().all(|a| !a.url.contains("spanified.com")));
    }

    #[test]
    fn falls_back_to_url_date() {
        let html = r#"<article class="story"><h2 class="story-title">T</h2>
            <div class="story-source"><a href="https://x.es/a">X</a></div></article>"#;
        let items = parse_issue(html, "https://spanified.com/archive/2026-07-01", "SD").unwrap();
        // 08:00 CEST = 06:00 UTC
        assert_eq!(items[0].published.to_rfc3339(), "2026-07-01T06:00:00+00:00");
    }
}
