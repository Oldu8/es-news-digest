//! `config.yaml` (everything you may want to tweak without recompiling) and
//! `.env` secrets.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::NaiveTime;
use chrono_tz::Tz;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schedule: ScheduleConfig,
    /// Only articles published within this many hours before the run are used.
    #[serde(default = "default_window_hours")]
    pub window_hours: i64,
    pub http: HttpConfig,
    pub dedup: DedupConfig,
    pub gemini: GeminiConfig,
    /// Fallback model when Gemini fails; absent = no fallback.
    #[serde(default)]
    pub openai: Option<OpenAiConfig>,
    pub digest: DigestConfig,
    pub topics: TopicsConfig,
    pub sources: Vec<SourceConfig>,
    pub paths: PathsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    /// Local time of the daily run, "HH:MM".
    pub time: String,
    /// IANA time zone, e.g. "Europe/Madrid".
    pub timezone: String,
    /// What `serve` does when a run fails (Gemini overloaded, network down…).
    #[serde(default)]
    pub retry: RetryConfig,
}

/// A failed run is repeated `attempts - 1` more times, `delay_minutes` apart;
/// the admin is alerted only when the last attempt fails too.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    pub attempts: u32,
    pub delay_minutes: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            attempts: 3,
            delay_minutes: 5,
        }
    }
}

impl ScheduleConfig {
    pub fn time(&self) -> Result<NaiveTime> {
        NaiveTime::parse_from_str(&self.time, "%H:%M")
            .with_context(|| format!("schedule.time must be HH:MM, got {:?}", self.time))
    }

    pub fn tz(&self) -> Result<Tz> {
        self.timezone
            .parse()
            .map_err(|e| anyhow::anyhow!("schedule.timezone {:?}: {e}", self.timezone))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    pub timeout_secs: u64,
    pub user_agent: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DedupConfig {
    /// Sørensen–Dice similarity of normalized titles (0..1) above which two
    /// articles are treated as the same story.
    pub title_similarity: f64,
    /// Sent URLs older than this are deleted from SQLite.
    pub retention_days: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeminiConfig {
    pub model: String,
    #[serde(default = "default_gemini_base")]
    pub base_url: String,
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Hard cap on the number of articles sent in one prompt (newest win).
    pub max_candidates: usize,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestConfig {
    /// Heading; `{date}` is replaced with e.g. "21 ноября".
    pub title: String,
    /// Blocks of the message, in display order. The model puts every story
    /// into one of them by `key`.
    pub sections: Vec<SectionConfig>,
    /// Items below this importance (1–5) are not shown.
    pub min_importance: u8,
    pub empty_text: String,
    /// Per-item summary cap in characters.
    pub summary_max_chars: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiConfig {
    pub model: String,
    #[serde(default = "default_openai_base")]
    pub base_url: String,
    /// For reasoning models: minimal / low / medium / high.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub timeout_secs: u64,
}

fn default_openai_base() -> String {
    "https://api.openai.com/v1".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionConfig {
    /// Stable id the model answers with, e.g. `spain`.
    pub key: String,
    /// Heading shown in the message, e.g. "🇪🇸 Испания".
    pub title: String,
    /// For the model: which stories belong here.
    pub description: String,
    pub max_items: usize,
}

impl DigestConfig {
    pub fn total_max_items(&self) -> usize {
        self.sections.iter().map(|s| s.max_items).sum()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopicsConfig {
    pub keep: Vec<String>,
    pub drop: Vec<String>,
}

/// One entry under `sources:`. `kind` picks the parser.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum SourceConfig {
    /// Spanified "Spain Daily": home page → today's /archive/YYYY-MM-DD issue.
    Spanified { name: String, url: String },
    /// One or more RSS 2.0 feeds.
    Rss { name: String, urls: Vec<String> },
    /// Google News sitemap; only URLs whose first path segment is in `sections`
    /// are kept (empty list = keep everything).
    Sitemap {
        name: String,
        url: String,
        #[serde(default)]
        sections: Vec<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathsConfig {
    pub database: PathBuf,
    pub log_dir: PathBuf,
}

fn default_window_hours() -> i64 {
    24
}

fn default_gemini_base() -> String {
    "https://generativelanguage.googleapis.com/v1beta".into()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&raw).with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let cfg: Config = serde_saphyr::from_str(raw)?;
        cfg.schedule.time()?;
        cfg.schedule.tz()?;
        if !(0.0..=1.0).contains(&cfg.dedup.title_similarity) {
            bail!("dedup.title_similarity must be within 0..1");
        }
        if cfg.schedule.retry.attempts == 0 {
            bail!("schedule.retry.attempts must be at least 1");
        }
        if cfg.digest.sections.is_empty() {
            bail!("digest.sections must list at least one section");
        }
        let mut keys = std::collections::HashSet::new();
        for sec in &cfg.digest.sections {
            if !keys.insert(sec.key.as_str()) {
                bail!("digest.sections: duplicate key {:?}", sec.key);
            }
        }
        if cfg.sources.is_empty() {
            bail!("no sources configured");
        }
        Ok(cfg)
    }
}

/// Secrets from the environment (`.env` is loaded by main).
#[derive(Clone)]
pub struct Secrets {
    pub gemini_api_key: Option<String>,
    /// Only needed for the OpenAI fallback.
    pub openai_api_key: Option<String>,
    pub telegram_bot_token: Option<String>,
    pub telegram_channel_id: Option<String>,
    pub telegram_admin_chat_id: Option<String>,
}

impl Secrets {
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Self {
            gemini_api_key: get("GEMINI_API_KEY"),
            openai_api_key: get("OPENAI_API_KEY"),
            telegram_bot_token: get("TELEGRAM_BOT_TOKEN"),
            telegram_channel_id: get("TELEGRAM_CHANNEL_ID"),
            telegram_admin_chat_id: get("TELEGRAM_ADMIN_CHAT_ID"),
        }
    }
}

// Never print secret values, even in debug logs.
impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = |o: &Option<String>| if o.is_some() { "set" } else { "missing" };
        f.debug_struct("Secrets")
            .field("gemini_api_key", &set(&self.gemini_api_key))
            .field("openai_api_key", &set(&self.openai_api_key))
            .field("telegram_bot_token", &set(&self.telegram_bot_token))
            .field("telegram_channel_id", &set(&self.telegram_channel_id))
            .field("telegram_admin_chat_id", &set(&self.telegram_admin_chat_id))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_config_parses() {
        let cfg = Config::parse(include_str!("../config.yaml")).expect("config.yaml is valid");
        assert_eq!(cfg.schedule.tz().unwrap(), chrono_tz::Europe::Madrid);
        assert!(cfg.sources.len() >= 5);
    }
}
