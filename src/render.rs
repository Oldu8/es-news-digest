//! Telegram HTML rendering of the digest.

use chrono::{Datelike, NaiveDate};

use crate::config::DigestConfig;

/// Telegram's limit for one message. Measured in UTF-16 code units to be safe
/// with emoji; we measure the HTML including tags, which over-estimates.
pub const TELEGRAM_LIMIT: usize = 4096;

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub name: String,
    pub url: String,
    /// Original (Spanish/English) title, remembered in SQLite for fuzzy dedup.
    pub original_title: String,
}

/// A finished digest entry: Russian text plus links to the originals.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub importance: u8,
    pub title: String,
    pub summary: String,
    pub links: Vec<Link>,
}

/// Escape text for Telegram's HTML parse mode (`&`, `<`, `>`; `"` too, so the
/// same helper works inside attributes).
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

const MONTHS: [&str; 12] = [
    "января",
    "февраля",
    "марта",
    "апреля",
    "мая",
    "июня",
    "июля",
    "августа",
    "сентября",
    "октября",
    "ноября",
    "декабря",
];

pub fn ru_date(d: NaiveDate) -> String {
    format!("{} {}", d.day(), MONTHS[d.month0() as usize])
}

fn tg_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn render_entry(e: &Entry, summary_max: usize) -> String {
    let marker = if e.importance >= 5 { "🔴" } else { "▪️" };
    let links = e
        .links
        .iter()
        .map(|l| format!("<a href=\"{}\">{}</a>", escape(&l.url), escape(&l.name)))
        .collect::<Vec<_>>()
        .join(" · ");
    let summary = crate::text::truncate_chars(e.summary.trim(), summary_max);
    let mut s = format!("{marker} <b>{}</b>", escape(e.title.trim()));
    if !summary.is_empty() {
        s.push('\n');
        s.push_str(&escape(&summary));
    }
    if !links.is_empty() {
        s.push('\n');
        s.push_str(&links);
    }
    s
}

/// The rendered message and the entries that made it in (in display order).
#[derive(Debug)]
pub struct Rendered<'a> {
    pub text: String,
    pub shown: Vec<&'a Entry>,
}

/// Pick, order and format the entries into one message that fits Telegram.
///
/// Entries below `min_importance` are dropped, the rest are sorted by
/// importance (stable, so the model's order breaks ties), capped at
/// `max_items`, and trailing entries are dropped until the text fits.
pub fn render<'a>(entries: &'a [Entry], date: NaiveDate, cfg: &DigestConfig) -> Rendered<'a> {
    let header = format!(
        "<b>{}</b>",
        escape(&cfg.title.replace("{date}", &ru_date(date)))
    );

    let mut chosen: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.importance >= cfg.min_importance)
        .collect();
    chosen.sort_by_key(|e| std::cmp::Reverse(e.importance));
    chosen.truncate(cfg.max_items);

    if chosen.is_empty() {
        return Rendered {
            text: format!("{header}\n\n{}", escape(&cfg.empty_text)),
            shown: chosen,
        };
    }

    let mut summary_max = cfg.summary_max_chars;
    loop {
        let body: Vec<String> = chosen
            .iter()
            .map(|e| render_entry(e, summary_max))
            .collect();
        let text = format!("{header}\n\n{}", body.join("\n\n"));
        if tg_len(&text) <= TELEGRAM_LIMIT {
            return Rendered {
                text,
                shown: chosen,
            };
        }
        if chosen.len() > 1 {
            chosen.pop();
        } else if summary_max > 40 {
            summary_max /= 2;
        } else {
            // One giant title: hard cut, tags may break, but it is unrealistic.
            return Rendered {
                text: text.chars().take(TELEGRAM_LIMIT / 2).collect(),
                shown: chosen,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DigestConfig {
        DigestConfig {
            title: "Испания: главное за {date}".into(),
            max_items: 7,
            min_importance: 2,
            empty_text: "Сегодня ничего важного.".into(),
            summary_max_chars: 280,
        }
    }

    fn entry(importance: u8, title: &str) -> Entry {
        Entry {
            importance,
            title: title.into(),
            summary: "Суть новости.".into(),
            links: vec![Link {
                name: "El Comercio".into(),
                url: "https://x.es/a?b=1&c=2".into(),
                original_title: "orig".into(),
            }],
        }
    }

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 21).unwrap()
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            escape("a < b & c > \"d\""),
            "a &lt; b &amp; c &gt; &quot;d&quot;"
        );
    }

    #[test]
    fn renders_sorted_with_red_marker_first() {
        let text = render(
            &[
                entry(3, "Третья"),
                entry(5, "IVA <важно>"),
                entry(1, "Мусор"),
                entry(4, "Четвёртая"),
            ],
            date(),
            &cfg(),
        )
        .text;
        let expected = "<b>Испания: главное за 21 ноября</b>\n\n\
            🔴 <b>IVA &lt;важно&gt;</b>\nСуть новости.\n<a href=\"https://x.es/a?b=1&amp;c=2\">El Comercio</a>\n\n\
            ▪️ <b>Четвёртая</b>\nСуть новости.\n<a href=\"https://x.es/a?b=1&amp;c=2\">El Comercio</a>\n\n\
            ▪️ <b>Третья</b>\nСуть новости.\n<a href=\"https://x.es/a?b=1&amp;c=2\">El Comercio</a>";
        assert_eq!(text, expected);
    }

    #[test]
    fn empty_digest() {
        assert_eq!(
            render(&[entry(1, "x")], date(), &cfg()).text,
            "<b>Испания: главное за 21 ноября</b>\n\nСегодня ничего важного."
        );
    }

    #[test]
    fn caps_items_and_fits_telegram_limit() {
        let mut c = cfg();
        c.max_items = 50;
        c.summary_max_chars = 1000;
        let long = Entry {
            summary: "слово ".repeat(150),
            ..entry(3, "Длинная")
        };
        let entries = vec![long; 20];
        let text = render(&entries, date(), &c).text;
        assert!(tg_len(&text) <= TELEGRAM_LIMIT);
        assert!(text.matches("<b>Длинная</b>").count() >= 2);

        let r = render(&entries, date(), &cfg());
        assert_eq!(r.shown.len(), 7);
        let text = r.text;
        assert!(text.matches("<b>Длинная</b>").count() <= 7);
    }

    #[test]
    fn several_links_joined() {
        let mut e = entry(4, "Дубль");
        e.links.push(Link {
            name: "LNE".into(),
            url: "https://lne.es/b".into(),
            original_title: "b".into(),
        });
        let text = render(&[e], date(), &cfg()).text;
        assert!(text.ends_with("El Comercio</a> · <a href=\"https://lne.es/b\">LNE</a>"));
    }
}
