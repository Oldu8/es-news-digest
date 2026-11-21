//! Duplicate removal before the LLM: same URL (after normalization) or a very
//! similar title. Cross-source grouping of the *same story told differently*
//! is left to Gemini.

use crate::article::Article;

/// Canonical form of a URL for comparisons: lowercase host without `www.`,
/// no fragment, no tracking parameters, no trailing slash.
pub fn normalize_url(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw.trim()) else {
        return raw.trim().trim_end_matches('/').to_lowercase();
    };
    url.set_fragment(None);
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| {
            let k = k.to_ascii_lowercase();
            !(k.starts_with("utm_")
                || matches!(
                    k.as_str(),
                    "fbclid"
                        | "gclid"
                        | "mc_cid"
                        | "mc_eid"
                        | "ref"
                        | "ns_campaign"
                        | "ns_mchannel"
                        | "int"
                ))
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(kept);
    }
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches("www.")
        .to_lowercase();
    let mut out = format!("{host}{}", url.path());
    if let Some(q) = url.query() {
        out.push('?');
        out.push_str(q);
    }
    out.trim_end_matches('/').to_string()
}

/// Lowercase, keep only letters/digits, single spaces. Accents are kept:
/// both sides come from Spanish sources, so they match anyway.
pub fn normalize_title(t: &str) -> String {
    t.chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn titles_similar(a: &str, b: &str, threshold: f64) -> bool {
    let (a, b) = (normalize_title(a), normalize_title(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || strsim::sorensen_dice(&a, &b) >= threshold
}

/// Remove duplicates, keeping the first occurrence (source order in the config
/// therefore decides who wins). When a later duplicate has a summary and the
/// kept one does not, the summary is carried over.
pub fn dedup(articles: Vec<Article>, threshold: f64) -> Vec<Article> {
    let mut kept: Vec<(String, String, Article)> = Vec::with_capacity(articles.len());
    'next: for a in articles {
        let url = normalize_url(&a.url);
        let title = normalize_title(&a.title);
        for (k_url, k_title, k) in kept.iter_mut() {
            let same = *k_url == url
                || (!title.is_empty()
                    && (*k_title == title || strsim::sorensen_dice(k_title, &title) >= threshold));
            if same {
                if k.summary.is_none() && a.summary.is_some() {
                    k.summary = a.summary;
                }
                continue 'next;
            }
        }
        kept.push((url, title, a));
    }
    kept.into_iter().map(|(_, _, a)| a).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn art(title: &str, url: &str, summary: Option<&str>) -> Article {
        Article {
            source: "t".into(),
            title: title.into(),
            summary: summary.map(Into::into),
            url: url.into(),
            published: Utc::now(),
            section: None,
        }
    }

    #[test]
    fn url_normalization() {
        assert_eq!(
            normalize_url("https://WWW.lne.es/oviedo/2026/a.html?utm_source=tw&utm_medium=x#top"),
            "lne.es/oviedo/2026/a.html"
        );
        assert_eq!(
            normalize_url("https://eldiario.es/economia/"),
            "eldiario.es/economia"
        );
        assert_eq!(
            normalize_url("https://x.es/a/?id=5&utm_campaign=c&fbclid=zz"),
            "x.es/a/?id=5"
        );
    }

    #[test]
    fn same_url_with_tracking_is_duplicate() {
        let out = dedup(
            vec![
                art("Uno", "https://x.es/a", None),
                art(
                    "Completely different",
                    "https://www.x.es/a/?utm_source=rss",
                    Some("lead"),
                ),
            ],
            0.8,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].summary.as_deref(), Some("lead"));
    }

    #[test]
    fn similar_titles_are_duplicates() {
        let out = dedup(
            vec![
                art(
                    "El Euríbor baja al 2,1% en noviembre",
                    "https://a.es/1",
                    None,
                ),
                art(
                    "El Euríbor baja al 2,1 % en noviembre.",
                    "https://b.es/2",
                    None,
                ),
                art("Sube la luz un 8% en diciembre", "https://c.es/3", None),
            ],
            0.8,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].url, "https://c.es/3");
    }

    #[test]
    fn threshold_controls_similarity() {
        let a = "Oviedo aprueba el nuevo plan de movilidad";
        let b = "Oviedo aprueba el plan de movilidad urbana";
        assert!(titles_similar(a, b, 0.6));
        assert!(!titles_similar(a, b, 0.99));
    }
}
