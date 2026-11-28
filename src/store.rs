//! SQLite memory of what was already posted, so the same story is not repeated
//! on the next days.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OpenFlags, params};

use crate::dedup::{normalize_url, titles_similar};

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct SentItem {
    pub url: String,
    pub title: String,
}

impl Store {
    /// Open (creating if needed) the database for a normal run.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    /// Open read-only for `--dry-run`. Returns `None` if the file does not exist.
    pub fn open_readonly(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {} read-only", path.display()))?;
        Ok(Some(Self { conn }))
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sent (
                 url     TEXT PRIMARY KEY,   -- normalized URL
                 title   TEXT NOT NULL,      -- original title, for fuzzy matching
                 sent_at INTEGER NOT NULL    -- unix seconds
             );
             CREATE INDEX IF NOT EXISTS sent_at_idx ON sent(sent_at);
             -- headlines of published digest items (in the digest language),
             -- shown to the model so it does not retell the same story
             CREATE TABLE IF NOT EXISTS stories (
                 title   TEXT NOT NULL,
                 sent_at INTEGER NOT NULL    -- unix seconds
             );
             CREATE INDEX IF NOT EXISTS stories_at_idx ON stories(sent_at);",
        )?;
        Ok(Self { conn })
    }

    pub fn recent(&self) -> Result<Vec<SentItem>> {
        let mut stmt = self.conn.prepare("SELECT url, title FROM sent")?;
        let rows = stmt.query_map([], |r| {
            Ok(SentItem {
                url: r.get(0)?,
                title: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Was this URL (or a very similar title) already posted?
    pub fn is_sent(sent: &[SentItem], url: &str, title: &str, threshold: f64) -> bool {
        let url = normalize_url(url);
        sent.iter()
            .any(|s| s.url == url || titles_similar(&s.title, title, threshold))
    }

    pub fn mark_sent<'a>(
        &mut self,
        items: impl IntoIterator<Item = (&'a str, &'a str)>,
        at: DateTime<Utc>,
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO sent (url, title, sent_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(url) DO UPDATE SET sent_at = excluded.sent_at",
            )?;
            for (url, title) in items {
                n += stmt.execute(params![normalize_url(url), title, at.timestamp()])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Remember the headlines of the items just published.
    pub fn mark_stories<'a>(
        &mut self,
        titles: impl IntoIterator<Item = &'a str>,
        at: DateTime<Utc>,
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare("INSERT INTO stories (title, sent_at) VALUES (?1, ?2)")?;
            for t in titles {
                n += stmt.execute(params![t, at.timestamp()])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Headlines published since `since`, oldest first.
    /// An old database opened read-only (`--dry-run`) may not have the table
    /// yet: then there is nothing to show.
    pub fn stories_since(&self, since: DateTime<Utc>) -> Result<Vec<String>> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'stories')",
            [],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare("SELECT title FROM stories WHERE sent_at >= ?1 ORDER BY sent_at, rowid")?;
        let rows = stmt.query_map(params![since.timestamp()], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Forget entries older than `days`. Returns how many rows were deleted.
    pub fn prune(&self, now: DateTime<Utc>, days: i64) -> Result<usize> {
        let cutoff = (now - Duration::days(days)).timestamp();
        let sent = self
            .conn
            .execute("DELETE FROM sent WHERE sent_at < ?1", params![cutoff])?;
        let stories = self
            .conn
            .execute("DELETE FROM stories WHERE sent_at < ?1", params![cutoff])?;
        Ok(sent + stories)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_and_prunes() {
        let mut s = Store::in_memory().unwrap();
        let now = Utc::now();
        s.mark_sent(
            [("https://x.es/a?utm_source=t", "Sube la luz")],
            now - Duration::days(40),
        )
        .unwrap();
        s.mark_sent([("https://x.es/b/", "Baja el Euríbor en noviembre")], now)
            .unwrap();
        let sent = s.recent().unwrap();
        assert!(Store::is_sent(&sent, "https://www.x.es/a", "otro", 0.8));
        assert!(Store::is_sent(
            &sent,
            "https://y.es/z",
            "Baja el Euríbor en noviembre.",
            0.8
        ));
        assert!(!Store::is_sent(
            &sent,
            "https://y.es/z",
            "Nada que ver",
            0.8
        ));

        s.mark_stories(["Стара історія"], now - Duration::days(40))
            .unwrap();
        s.mark_stories(["Інфляція 4,1%", "Вибори"], now).unwrap();
        assert_eq!(
            s.stories_since(now - Duration::hours(48)).unwrap(),
            ["Інфляція 4,1%", "Вибори"]
        );

        assert_eq!(s.prune(now, 30).unwrap(), 2);
        let sent = s.recent().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].url, "x.es/b");
    }
}
