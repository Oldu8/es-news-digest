use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use quick_xml::events::Event;

use super::{Source, SourceError, SourceResult, get_text, section_of};
use crate::article::Article;
use crate::text::squash_ws;

/// Google News sitemap (`<url><loc>…<news:news>…</news:news></url>`).
pub struct SitemapSource {
    pub name: String,
    pub url: String,
    pub sections: Vec<String>,
}

impl Source for SitemapSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn fetch<'a>(
        &'a self,
        http: &'a reqwest::Client,
        since: DateTime<Utc>,
    ) -> BoxFuture<'a, SourceResult<Vec<Article>>> {
        Box::pin(async move {
            let body = get_text(http, &self.url).await?;
            let mut items = parse_sitemap(&body, &self.name, &self.sections)?;
            items.retain(|a| a.published >= since);
            Ok(items)
        })
    }
}

#[derive(Default)]
struct Entry {
    loc: String,
    title: String,
    date: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    None,
    Loc,
    Title,
    Date,
}

/// Parse a Google News sitemap with a streaming XML reader. Keeps only URLs
/// whose first path segment is in `sections` (all of them if it is empty).
pub fn parse_sitemap(xml: &str, source: &str, sections: &[String]) -> SourceResult<Vec<Article>> {
    let parse_err = |detail: String| SourceError::Parse {
        what: format!("{source} sitemap"),
        detail,
    };

    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    let mut entry: Option<Entry> = None;
    let mut field = Field::None;
    let mut saw_urlset = false;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| parse_err(format!("at byte {}: {e}", reader.buffer_position())))?;
        match event {
            Event::Start(e) => match e.local_name().as_ref() {
                "urlset" => saw_urlset = true,
                "url" => entry = Some(Entry::default()),
                // <loc> also appears inside <image:image>; we only want the one
                // that is a direct child of <url>, which comes first.
                "loc" if entry.as_ref().is_some_and(|e| e.loc.is_empty()) => field = Field::Loc,
                "title" if e.name().as_ref() == "news:title" => field = Field::Title,
                "publication_date" if e.name().as_ref() == "news:publication_date" => {
                    field = Field::Date
                }
                _ => {}
            },
            Event::End(e) => {
                if e.local_name().as_ref() == "url"
                    && let Some(en) = entry.take()
                    && let Some(a) = to_article(en, source, sections)
                {
                    out.push(a);
                }
                field = Field::None;
            }
            Event::Text(t) => push(&mut entry, field, &t.xml10_content()),
            Event::CData(t) => push(&mut entry, field, &t.xml10_content()),
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref() {
                    Ok(Some(c)) => c.to_string(),
                    _ => match r.as_ref() {
                        "amp" => "&".into(),
                        "lt" => "<".into(),
                        "gt" => ">".into(),
                        "quot" => "\"".into(),
                        "apos" => "'".into(),
                        _ => String::new(),
                    },
                };
                push(&mut entry, field, &resolved);
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if !saw_urlset {
        return Err(parse_err(
            "no <urlset> element (blocked or not a sitemap?)".into(),
        ));
    }
    Ok(out)
}

fn push(entry: &mut Option<Entry>, field: Field, text: &str) {
    let Some(e) = entry.as_mut() else { return };
    match field {
        Field::Loc => e.loc.push_str(text),
        Field::Title => e.title.push_str(text),
        Field::Date => e.date.push_str(text),
        Field::None => {}
    }
}

fn to_article(e: Entry, source: &str, sections: &[String]) -> Option<Article> {
    let url = e.loc.trim().to_string();
    let section = section_of(&url);
    if !sections.is_empty() && !section.as_ref().is_some_and(|s| sections.contains(s)) {
        return None;
    }
    let published = DateTime::parse_from_rfc3339(e.date.trim())
        .ok()?
        .with_timezone(&Utc);
    let title = squash_ws(&e.title);
    (!title.is_empty()).then(|| Article {
        source: source.to_string(),
        title,
        summary: None,
        url,
        published,
        section,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const XML: &str = include_str!("../../tests/fixtures/lne_sitemap.xml");

    #[test]
    fn parses_all_entries_without_filter() {
        let items = parse_sitemap(XML, "LNE", &[]).unwrap();
        assert_eq!(items.len(), 7);
        let a = &items[0];
        assert_eq!(
            a.title,
            "Alcaraz sufre y remonta ante Shelton para sellar su pase a la final"
        );
        assert_eq!(a.section.as_deref(), Some("deportes"));
        assert_eq!(
            a.published,
            Utc.with_ymd_and_hms(2026, 11, 21, 13, 0, 56).unwrap()
        );
    }

    #[test]
    fn filters_by_section() {
        let sections = vec!["oviedo".to_string(), "economia".into(), "asturias".into()];
        let items = parse_sitemap(XML, "LNE", &sections).unwrap();
        let got: Vec<_> = items.iter().map(|a| a.section.clone().unwrap()).collect();
        assert_eq!(got, ["oviedo", "asturias", "economia"]);
        assert!(items[1].title.contains("\"agenda oculta\""));
    }

    #[test]
    fn waf_page_is_an_error() {
        let html =
            r#"<?xml version="1.0"?><html><head><title>406 WAF Forbidden</title></head></html>"#;
        assert!(parse_sitemap(html, "LNE", &[]).is_err());
    }
}
