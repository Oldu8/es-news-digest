//! One digest run: collect → dedup → (skip already sent) → Gemini → render → send.

use std::collections::HashSet;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use futures::future::join_all;
use tracing::{info, warn};

use crate::article::Article;
use crate::config::{Config, Secrets};
use crate::dedup::{dedup, normalize_url};
use crate::gemini::{Gemini, LlmItem};
use crate::render::{Entry, Link, render};
use crate::sources;
use crate::store::{SentItem, Store};
use crate::telegram::Telegram;

pub fn http_client(cfg: &Config) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(&cfg.http.user_agent)
        .timeout(StdDuration::from_secs(cfg.http.timeout_secs))
        .build()?)
}

/// Fetch every source concurrently. A failing source is logged and skipped;
/// only if *all* of them fail is this an error.
pub async fn collect(
    cfg: &Config,
    http: &reqwest::Client,
    now: DateTime<Utc>,
) -> Result<Vec<Article>> {
    let since = now - Duration::hours(cfg.window_hours);
    let sources = sources::from_config(&cfg.sources);
    let results = join_all(sources.iter().map(|s| s.fetch(http, since))).await;

    let mut all = Vec::new();
    let mut failed = 0;
    for (src, res) in sources.iter().zip(results) {
        match res {
            Ok(items) => {
                info!(source = src.name(), articles = items.len(), "source ok");
                all.extend(items);
            }
            Err(e) => {
                failed += 1;
                warn!(source = src.name(), error = %e, "source failed, skipping");
            }
        }
    }
    if failed == sources.len() {
        bail!("all {failed} sources failed");
    }
    Ok(all)
}

/// Dedup, drop already-posted stories, keep the newest `max_candidates`.
pub fn prepare(articles: Vec<Article>, cfg: &Config, sent: &[SentItem]) -> Vec<Article> {
    let total = articles.len();
    let mut items = dedup(articles, cfg.dedup.title_similarity);
    let after_dedup = items.len();
    items.retain(|a| !Store::is_sent(sent, &a.url, &a.title, cfg.dedup.title_similarity));
    items.sort_by_key(|a| std::cmp::Reverse(a.published));
    items.truncate(cfg.gemini.max_candidates);
    info!(
        total,
        after_dedup,
        candidates = items.len(),
        "prepared candidates"
    );
    items
}

/// Turn model answers into renderable entries, resolving ids to links.
/// Unknown ids are ignored; items without any valid id are dropped.
pub fn to_entries(items: Vec<LlmItem>, articles: &[Article]) -> Vec<Entry> {
    items
        .into_iter()
        .filter_map(|it| {
            let mut seen = HashSet::new();
            let links: Vec<Link> = it
                .ids
                .iter()
                .filter_map(|&id| articles.get(id))
                .filter(|a| seen.insert(normalize_url(&a.url)))
                .map(|a| Link {
                    name: a.source.clone(),
                    url: a.url.clone(),
                    original_title: a.title.clone(),
                })
                .collect();
            if links.is_empty() {
                warn!(title = %it.title_ru, ids = ?it.ids, "model returned unknown ids, dropping item");
                return None;
            }
            Some(Entry {
                importance: it.importance,
                title: it.title_ru,
                summary: it.summary_ru,
                links,
            })
        })
        .collect()
}

/// Run the whole pipeline once. Returns the message text.
pub async fn run(cfg: &Config, secrets: &Secrets, dry_run: bool) -> Result<String> {
    let now = Utc::now();
    let tz = cfg.schedule.tz()?;
    let gemini_key = secrets
        .gemini_api_key
        .clone()
        .context("GEMINI_API_KEY is not set (see .env.example)")?;
    // Fail fast on missing Telegram settings before spending a Gemini call.
    let tg = if dry_run {
        None
    } else {
        let token = secrets
            .telegram_bot_token
            .clone()
            .context("TELEGRAM_BOT_TOKEN is not set")?;
        let channel = secrets
            .telegram_channel_id
            .clone()
            .context("TELEGRAM_CHANNEL_ID is not set")?;
        Some((Telegram::new(token)?, channel))
    };

    // Dry run must not write to SQLite: open read-only (or not at all).
    let mut store = if dry_run {
        Store::open_readonly(&cfg.paths.database)?
    } else {
        Some(Store::open(&cfg.paths.database)?)
    };
    let sent = match &store {
        Some(s) => s.recent()?,
        None => Vec::new(),
    };

    let http = http_client(cfg)?;
    let articles = collect(cfg, &http, now).await?;
    let candidates = prepare(articles, cfg, &sent);

    let entries = if candidates.is_empty() {
        info!("no candidates, skipping Gemini");
        Vec::new()
    } else {
        let gemini = Gemini::new(cfg.gemini.clone(), gemini_key)?;
        let items = gemini
            .select(&candidates, &cfg.topics, cfg.digest.max_items)
            .await
            .context("Gemini step failed")?;
        info!(items = items.len(), "Gemini selected");
        to_entries(items, &candidates)
    };

    let rendered = render(&entries, now.with_timezone(&tz).date_naive(), &cfg.digest);
    info!(
        shown = rendered.shown.len(),
        chars = rendered.text.chars().count(),
        "rendered digest"
    );

    if let Some((tg, channel)) = tg {
        tg.send(&channel, &rendered.text, true)
            .await
            .context("posting the digest to the channel")?;
        info!(%channel, "digest posted");
        if let Some(store) = store.as_mut() {
            let links = rendered.shown.iter().flat_map(|e| e.links.iter());
            let n = store.mark_sent(
                links.map(|l| (l.url.as_str(), l.original_title.as_str())),
                now,
            )?;
            let pruned = store.prune(now, cfg.dedup.retention_days)?;
            info!(remembered = n, pruned, "store updated");
        }
    }
    Ok(rendered.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art(i: usize) -> Article {
        Article {
            source: format!("S{i}"),
            title: format!("Title {i}"),
            summary: None,
            url: format!("https://x.es/{i}"),
            published: Utc::now(),
            section: None,
        }
    }

    #[test]
    fn ids_resolve_to_unique_links() {
        let mut dup = art(0);
        dup.url = "https://www.x.es/0/?utm_source=a".into();
        let arts = vec![art(0), art(1), dup];
        let items = vec![
            LlmItem {
                ids: vec![0, 2, 1, 99],
                topic: "t".into(),
                importance: 4,
                title_ru: "Т".into(),
                summary_ru: "С".into(),
            },
            LlmItem {
                ids: vec![42],
                topic: "t".into(),
                importance: 5,
                title_ru: "Призрак".into(),
                summary_ru: "".into(),
            },
        ];
        let entries = to_entries(items, &arts);
        assert_eq!(entries.len(), 1);
        let urls: Vec<_> = entries[0].links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(urls, ["https://x.es/0", "https://x.es/1"]);
    }
}
