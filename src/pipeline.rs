//! One digest run: collect → dedup → (skip already sent) → Gemini → render → send.

use std::collections::HashSet;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use futures::future::join_all;
use tracing::{debug, info, warn};

use crate::article::Article;
use crate::config::{Config, Secrets, TopicsConfig};
use crate::dedup::{dedup, normalize_url, titles_similar};
use crate::gemini::{Gemini, LlmItem};
use crate::openai::OpenAi;
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

/// Drop articles whose URL date is older than the window (with a day of slack
/// for time zones), dedup, drop already-posted stories, keep the newest
/// `max_candidates`.
pub fn prepare(
    articles: Vec<Article>,
    cfg: &Config,
    sent: &[SentItem],
    now: DateTime<Utc>,
) -> Vec<Article> {
    let total = articles.len();
    let oldest = (now - Duration::hours(cfg.window_hours)).date_naive() - Duration::days(1);
    let (fresh, stale): (Vec<_>, Vec<_>) = articles
        .into_iter()
        .partition(|a| sources::url_date(&a.url).is_none_or(|d| d >= oldest));
    for a in &stale {
        debug!(url = %a.url, "stale by URL date, skipping");
    }
    let mut items = dedup(fresh, cfg.dedup.title_similarity);
    let after_dedup = items.len();
    items.retain(|a| !Store::is_sent(sent, &a.url, &a.title, cfg.dedup.title_similarity));
    items.sort_by_key(|a| std::cmp::Reverse(a.published));
    items.truncate(cfg.gemini.max_candidates);
    info!(
        total,
        stale = stale.len(),
        after_dedup,
        candidates = items.len(),
        "prepared candidates"
    );
    items
}

/// Drop stories that were already published: the ones the model marked with
/// `repeat_of`, plus (as a backstop, the model is not always careful) those
/// whose headline is nearly the same as a published one.
pub fn drop_repeats(items: Vec<LlmItem>, published: &[String], threshold: f64) -> Vec<LlmItem> {
    items
        .into_iter()
        .filter(|it| {
            let marked = usize::try_from(it.repeat_of)
                .ok()
                .and_then(|i| published.get(i));
            let similar = published
                .iter()
                .find(|p| titles_similar(p, &it.title, threshold));
            match marked.or(similar) {
                Some(p) => {
                    info!(title = %it.title, published = %p, "already published, skipping");
                    false
                }
                None => true,
            }
        })
        .collect()
}

/// Turn model answers into renderable entries, resolving ids to links and the
/// first topic to its hashtag. Unknown ids are ignored; items without any
/// valid id are dropped.
pub fn to_entries(items: Vec<LlmItem>, articles: &[Article], topics: &TopicsConfig) -> Vec<Entry> {
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
                warn!(title = %it.title, ids = ?it.ids, "model returned unknown ids, dropping item");
                return None;
            }
            let tag = it
                .topics
                .iter()
                .find_map(|k| topics.catalog.iter().find(|t| &t.key == k))
                .map(|t| t.tag.clone());
            Some(Entry {
                section: it.section,
                tag,
                topics: it.topics,
                importance: it.importance,
                title: it.title,
                summary: it.summary,
                links,
            })
        })
        .collect()
}

/// Ask Gemini; if it fails and the OpenAI fallback is configured, ask OpenAI
/// with the same prompt right away, so the digest still goes out on time.
async fn select_items(
    cfg: &Config,
    secrets: &Secrets,
    gemini_key: String,
    candidates: &[Article],
    published: &[String],
) -> Result<Vec<LlmItem>> {
    let sections = &cfg.digest.sections;
    let lang = cfg.digest.language;
    let gemini = Gemini::new(cfg.gemini.clone(), gemini_key)?;
    let gemini_err = match gemini
        .select(candidates, &cfg.topics, sections, lang, published)
        .await
    {
        Ok(items) => return Ok(items),
        Err(e) => e.context("Gemini step failed"),
    };
    let (Some(oc), Some(key)) = (&cfg.openai, &secrets.openai_api_key) else {
        return Err(gemini_err);
    };
    warn!("{gemini_err:#}; falling back to OpenAI");
    let openai = OpenAi::new(oc.clone(), key.clone())?;
    openai
        .select(candidates, &cfg.topics, sections, lang, published)
        .await
        .with_context(|| format!("OpenAI fallback failed too ({gemini_err:#})"))
}

/// Marker in the error chain: the digest is already in the channel, so the run
/// must not be retried (that would post it twice); only bookkeeping failed.
#[derive(Debug)]
pub struct AlreadyPosted;

impl std::fmt::Display for AlreadyPosted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the digest was posted, but remembering the sent links failed")
    }
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
    let story_since = now - Duration::hours(cfg.dedup.story_memory_hours);
    let (sent, published) = match &store {
        Some(s) => (s.recent()?, s.stories_since(story_since)?),
        None => (Vec::new(), Vec::new()),
    };

    let http = http_client(cfg)?;
    let articles = collect(cfg, &http, now).await?;
    let candidates = prepare(articles, cfg, &sent, now);

    let entries = if candidates.is_empty() {
        info!("no candidates, skipping Gemini");
        Vec::new()
    } else {
        info!(
            published = published.len(),
            "recent headlines for the model"
        );
        let items = select_items(cfg, secrets, gemini_key, &candidates, &published).await?;
        info!(items = items.len(), "model selected");
        let items = drop_repeats(items, &published, cfg.dedup.title_similarity);
        to_entries(items, &candidates, &cfg.topics)
    };

    let rendered = render(&entries, now.with_timezone(&tz).naive_local(), &cfg.digest);
    info!(
        shown = rendered.shown.len(),
        messages = rendered.messages.len(),
        chars = rendered
            .messages
            .iter()
            .map(|m| m.chars().count())
            .sum::<usize>(),
        "rendered digest"
    );

    if let Some((tg, channel)) = tg {
        // Once the first message is out, a failure must not trigger a retry
        // (that would repeat the beginning): remember what was sent and stop.
        let mut send_err = None;
        for (i, msg) in rendered.messages.iter().enumerate() {
            if let Err(e) = tg.send(&channel, msg, true).await {
                let e = e.context(format!(
                    "posting message {} of {} to the channel",
                    i + 1,
                    rendered.messages.len()
                ));
                if i == 0 {
                    return Err(e);
                }
                send_err = Some(e.context(AlreadyPosted));
                break;
            }
        }
        info!(%channel, partial = send_err.is_some(), "digest posted");
        if let Some(store) = store.as_mut() {
            let links = rendered.shown.iter().flat_map(|e| e.links.iter());
            let n = store
                .mark_sent(
                    links.map(|l| (l.url.as_str(), l.original_title.as_str())),
                    now,
                )
                .context(AlreadyPosted)?;
            store
                .mark_stories(rendered.shown.iter().map(|e| e.title.as_str()), now)
                .context(AlreadyPosted)?;
            let pruned = store
                .prune(now, cfg.dedup.retention_days)
                .context(AlreadyPosted)?;
            info!(remembered = n, pruned, "store updated");
        }
        if let Some(e) = send_err {
            return Err(e);
        }
    }
    Ok(rendered.messages.join("\n\n———— next message ————\n\n"))
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

    fn item(title: &str, repeat_of: i64) -> LlmItem {
        LlmItem {
            ids: vec![0],
            section: "spain".into(),
            topics: vec![],
            importance: 3,
            title: title.into(),
            summary: String::new(),
            repeat_of,
        }
    }

    #[test]
    fn repeats_are_dropped() {
        let published = vec![
            "Інфляція в Іспанії знизилася до 4,1% у листопаді".to_string(),
            "Вибори 29 листопада".to_string(),
        ];
        let items = vec![
            item("Нове про вибори", 1),                                    // marked
            item("Інфляція в Іспанії знизилася до 4,1% у листопаді.", -1), // near-identical
            item("Світло дешевшає з грудня", -1),                          // new
            item("Номер поза списком", 7),                                 // bogus index = new
        ];
        let kept: Vec<String> = drop_repeats(items, &published, 0.8)
            .into_iter()
            .map(|i| i.title)
            .collect();
        assert_eq!(kept, ["Світло дешевшає з грудня", "Номер поза списком"]);
    }

    #[test]
    fn ids_resolve_to_unique_links() {
        let mut dup = art(0);
        dup.url = "https://www.x.es/0/?utm_source=a".into();
        let arts = vec![art(0), art(1), dup];
        let items = vec![
            LlmItem {
                ids: vec![0, 2, 1, 99],
                section: "spain".into(),
                topics: vec!["taxes".into()],
                importance: 4,
                title: "Т".into(),
                summary: "С".into(),
                repeat_of: -1,
            },
            LlmItem {
                ids: vec![42],
                section: "spain".into(),
                topics: vec![],
                importance: 5,
                title: "Привид".into(),
                summary: "".into(),
                repeat_of: -1,
            },
        ];
        let topics = TopicsConfig {
            catalog: vec![crate::config::TopicConfig {
                key: "taxes".into(),
                title: "Податки".into(),
                tag: "податки".into(),
                description: String::new(),
            }],
            drop: vec![],
        };
        let entries = to_entries(items, &arts, &topics);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tag.as_deref(), Some("податки"));
        let urls: Vec<_> = entries[0].links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(urls, ["https://x.es/0", "https://x.es/1"]);
    }
}
