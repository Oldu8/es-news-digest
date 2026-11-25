//! Telegram HTML rendering of the digest.

use chrono::{Datelike, NaiveDate};

use crate::config::DigestConfig;

/// Telegram's limit for one message: 4096 characters of the text *after*
/// entity parsing, i.e. without tags and link URLs. Measured in UTF-16 code
/// units to be safe with emoji.
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
    /// Key of the digest section; unknown keys land in the first section.
    pub section: String,
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

/// Length of the message as Telegram counts it: tags (with their `href`s)
/// stripped and our four entities decoded.
fn tg_len(html: &str) -> usize {
    let mut visible = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => visible.push(c),
            _ => {}
        }
    }
    visible
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .encode_utf16()
        .count()
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
/// Entries below `min_importance` are dropped, the rest are grouped into the
/// configured sections (in config order), sorted by importance inside each
/// (stable, so the model's order breaks ties) and capped at the section's
/// `max_items`. Empty sections are not shown. While the text is too long, the
/// least important last entry of some section is dropped.
pub fn render<'a>(entries: &'a [Entry], date: NaiveDate, cfg: &DigestConfig) -> Rendered<'a> {
    let header = format!(
        "<b>{}</b>",
        escape(&cfg.title.replace("{date}", &ru_date(date)))
    );

    let mut groups: Vec<Vec<&Entry>> = vec![Vec::new(); cfg.sections.len()];
    for e in entries
        .iter()
        .filter(|e| e.importance >= cfg.min_importance)
    {
        let i = cfg
            .sections
            .iter()
            .position(|s| s.key == e.section)
            .unwrap_or(0);
        groups[i].push(e);
    }
    for (group, sec) in groups.iter_mut().zip(&cfg.sections) {
        group.sort_by_key(|e| std::cmp::Reverse(e.importance));
        group.truncate(sec.max_items);
    }

    let shown = |groups: &[Vec<&'a Entry>]| groups.iter().flatten().copied().collect::<Vec<_>>();
    if groups.iter().all(Vec::is_empty) {
        return Rendered {
            text: format!("{header}\n\n{}", escape(&cfg.empty_text)),
            shown: Vec::new(),
        };
    }

    let mut summary_max = cfg.summary_max_chars;
    loop {
        let mut text = header.clone();
        for (group, sec) in groups.iter().zip(&cfg.sections) {
            if group.is_empty() {
                continue;
            }
            text.push_str(&format!("\n\n<b>{}</b>", escape(&sec.title)));
            for e in group {
                text.push_str("\n\n");
                text.push_str(&render_entry(e, summary_max));
            }
        }
        if tg_len(&text) <= TELEGRAM_LIMIT {
            return Rendered {
                text,
                shown: shown(&groups),
            };
        }
        let total: usize = groups.iter().map(Vec::len).sum();
        if total > 1 {
            // The weakest tail; on a tie, the biggest section gives one up.
            let victim = groups
                .iter()
                .enumerate()
                .filter_map(|(i, g)| g.last().map(|e| (i, e.importance, g.len())))
                .min_by_key(|&(_, imp, len)| (imp, std::cmp::Reverse(len)))
                .map(|(i, _, _)| i)
                .expect("total > 1 means some group is non-empty");
            groups[victim].pop();
        } else if summary_max > 40 {
            summary_max /= 2;
        } else {
            // One giant title: hard cut, tags may break, but it is unrealistic.
            return Rendered {
                text: text.chars().take(TELEGRAM_LIMIT / 2).collect(),
                shown: shown(&groups),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SectionConfig;

    fn section(key: &str, title: &str, max_items: usize) -> SectionConfig {
        SectionConfig {
            key: key.into(),
            title: title.into(),
            description: String::new(),
            max_items,
        }
    }

    fn cfg() -> DigestConfig {
        DigestConfig {
            title: "Главное за {date}".into(),
            sections: vec![section("spain", "Испания", 7)],
            min_importance: 2,
            empty_text: "Сегодня ничего важного.".into(),
            summary_max_chars: 280,
        }
    }

    fn entry(importance: u8, title: &str) -> Entry {
        Entry {
            section: "spain".into(),
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
        let expected = "<b>Главное за 21 ноября</b>\n\n<b>Испания</b>\n\n\
            🔴 <b>IVA &lt;важно&gt;</b>\nСуть новости.\n<a href=\"https://x.es/a?b=1&amp;c=2\">El Comercio</a>\n\n\
            ▪️ <b>Четвёртая</b>\nСуть новости.\n<a href=\"https://x.es/a?b=1&amp;c=2\">El Comercio</a>\n\n\
            ▪️ <b>Третья</b>\nСуть новости.\n<a href=\"https://x.es/a?b=1&amp;c=2\">El Comercio</a>";
        assert_eq!(text, expected);
    }

    #[test]
    fn empty_digest() {
        assert_eq!(
            render(&[entry(1, "x")], date(), &cfg()).text,
            "<b>Главное за 21 ноября</b>\n\nСегодня ничего важного."
        );
    }

    #[test]
    fn caps_items_and_fits_telegram_limit() {
        let mut c = cfg();
        c.sections[0].max_items = 50;
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

    #[test]
    fn groups_into_sections_in_config_order() {
        let mut c = cfg();
        c.sections = vec![
            section("spain", "🇪🇸 Испания", 2),
            section("asturias", "Астурия", 2),
        ];
        let ast = |imp, t: &str| Entry {
            section: "asturias".into(),
            ..entry(imp, t)
        };
        let entries = [
            ast(3, "Овьедо"),
            entry(2, "Мадрид-2"),
            ast(5, "Сьеро"),
            entry(4, "Мадрид-4"),
            entry(3, "Мадрид-3"),
            Entry {
                section: "mars".into(),
                ..entry(5, "Неизвестный раздел")
            },
        ];
        let r = render(&entries, date(), &c);
        let titles: Vec<&str> = r.shown.iter().map(|e| e.title.as_str()).collect();
        // unknown section goes to the first one; each section capped at 2
        assert_eq!(
            titles,
            ["Неизвестный раздел", "Мадрид-4", "Сьеро", "Овьедо"]
        );
        let spain = r.text.find("🇪🇸 Испания").unwrap();
        let asturias = r.text.find("<b>Астурия</b>").unwrap();
        assert!(spain < asturias && r.text.find("Сьеро").unwrap() > asturias);
    }

    #[test]
    fn empty_section_is_hidden() {
        let mut c = cfg();
        c.sections.push(section("asturias", "Астурия", 3));
        let text = render(&[entry(4, "Мадрид")], date(), &c).text;
        assert!(text.contains("<b>Испания</b>"));
        assert!(!text.contains("Астурия"));
    }

    #[test]
    fn length_ignores_tags_and_urls() {
        assert_eq!(
            tg_len("<b>a &amp; b</b> <a href=\"https://very.long/url\">x</a>"),
            "a & b x".len()
        );
    }
}
