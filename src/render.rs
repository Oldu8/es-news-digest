//! Telegram HTML rendering of the digest.

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};

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

/// The digest as one or more Telegram messages, and the entries in them (in
/// display order).
#[derive(Debug)]
pub struct Rendered<'a> {
    pub messages: Vec<String>,
    pub shown: Vec<&'a Entry>,
}

/// "Утро" before 14:00 local time, "Вечер" after: for `{period}` in the title.
pub fn period(when: NaiveDateTime) -> &'static str {
    if when.hour() < 14 {
        "Утро"
    } else {
        "Вечер"
    }
}

/// Pick, order and format the entries.
///
/// Entries below `min_importance` are dropped, the rest are grouped into the
/// configured sections (in config order), sorted by importance inside each
/// (stable, so the model's order breaks ties) and capped at the section's
/// `max_items`. Empty sections are not shown. Nothing is dropped for length:
/// when the text does not fit one Telegram message, it continues in the next
/// one, split between entries, with the section heading repeated.
pub fn render<'a>(entries: &'a [Entry], when: NaiveDateTime, cfg: &DigestConfig) -> Rendered<'a> {
    let title = cfg
        .title
        .replace("{date}", &ru_date(when.date()))
        .replace("{period}", period(when));
    let header = format!("<b>{}</b>", escape(&title));

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
    let shown: Vec<&Entry> = groups.iter().flatten().copied().collect();
    if shown.is_empty() {
        return Rendered {
            messages: vec![format!("{header}\n\n{}", escape(&cfg.empty_text))],
            shown,
        };
    }

    let mut messages = Vec::new();
    let mut current = header;
    for (group, sec) in groups.iter().zip(&cfg.sections) {
        let heading = format!("<b>{}</b>", escape(&sec.title));
        for (j, e) in group.iter().enumerate() {
            let entry = render_entry(e, cfg.summary_max_chars);
            // A section heading never ends a message on its own.
            let block = if j == 0 {
                format!("{heading}\n\n{entry}")
            } else {
                entry.clone()
            };
            let candidate = format!("{current}\n\n{block}");
            if tg_len(&candidate) <= TELEGRAM_LIMIT {
                current = candidate;
                continue;
            }
            messages.push(std::mem::take(&mut current));
            current = format!("{heading}\n\n{entry}");
            if tg_len(&current) > TELEGRAM_LIMIT {
                // One entry over 4096 visible chars: unrealistic, hard cut.
                current = current.chars().take(TELEGRAM_LIMIT / 2).collect();
            }
        }
    }
    messages.push(current);
    Rendered { messages, shown }
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
            title: "{period}: главное за {date}".into(),
            sections: vec![section("spain", "Испания", 7)],
            min_importance: 2,
            empty_text: "Сегодня ничего важного.".into(),
            summary_max_chars: 600,
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

    fn morning() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 21)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap()
    }

    fn one(r: Rendered) -> String {
        assert_eq!(r.messages.len(), 1);
        r.messages.into_iter().next().unwrap()
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
        let text = one(render(
            &[
                entry(3, "Третья"),
                entry(5, "IVA <важно>"),
                entry(1, "Мусор"),
                entry(4, "Четвёртая"),
            ],
            morning(),
            &cfg(),
        ));
        let expected = "<b>Утро: главное за 21 ноября</b>\n\n<b>Испания</b>\n\n\
            🔴 <b>IVA &lt;важно&gt;</b>\nСуть новости.\n<a href=\"https://x.es/a?b=1&amp;c=2\">El Comercio</a>\n\n\
            ▪️ <b>Четвёртая</b>\nСуть новости.\n<a href=\"https://x.es/a?b=1&amp;c=2\">El Comercio</a>\n\n\
            ▪️ <b>Третья</b>\nСуть новости.\n<a href=\"https://x.es/a?b=1&amp;c=2\">El Comercio</a>";
        assert_eq!(text, expected);
    }

    #[test]
    fn evening_title_and_empty_digest() {
        let evening = morning().date().and_hms_opt(18, 0, 0).unwrap();
        assert_eq!(
            one(render(&[entry(1, "x")], evening, &cfg())),
            "<b>Вечер: главное за 21 ноября</b>\n\nСегодня ничего важного."
        );
    }

    #[test]
    fn long_digest_splits_into_messages_without_dropping() {
        let mut c = cfg();
        c.sections = vec![
            section("spain", "Испания", 50),
            section("asturias", "Астурия", 50),
        ];
        let long = |sec: &str, t: &str| Entry {
            section: sec.into(),
            summary: "слово ".repeat(90),
            ..entry(3, t)
        };
        let mut entries: Vec<Entry> = (0..12).map(|i| long("spain", &format!("И{i}"))).collect();
        entries.extend((0..12).map(|i| long("asturias", &format!("А{i}"))));
        let r = render(&entries, morning(), &c);
        assert_eq!(r.shown.len(), 24, "nothing dropped");
        assert!(r.messages.len() >= 2);
        for m in &r.messages {
            assert!(tg_len(m) <= TELEGRAM_LIMIT);
            // every message starts with a heading, never ends with one
            assert!(m.starts_with("<b>"));
            assert!(!m.ends_with("</b>"));
        }
        let all = r.messages.join("\n");
        for i in 0..12 {
            assert!(all.contains(&format!("<b>И{i}</b>")) && all.contains(&format!("<b>А{i}</b>")));
        }
        assert!(r.messages[0].starts_with("<b>Утро: главное"));
    }

    #[test]
    fn caps_per_section() {
        let mut c = cfg();
        c.sections[0].max_items = 2;
        let entries = [entry(3, "a"), entry(4, "b"), entry(5, "c")];
        let r = render(&entries, morning(), &c);
        let titles: Vec<&str> = r.shown.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["c", "b"]);
    }

    #[test]
    fn several_links_joined() {
        let mut e = entry(4, "Дубль");
        e.links.push(Link {
            name: "LNE".into(),
            url: "https://lne.es/b".into(),
            original_title: "b".into(),
        });
        let text = one(render(&[e], morning(), &cfg()));
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
        let r = render(&entries, morning(), &c);
        let titles: Vec<&str> = r.shown.iter().map(|e| e.title.as_str()).collect();
        // unknown section goes to the first one; each section capped at 2
        assert_eq!(
            titles,
            ["Неизвестный раздел", "Мадрид-4", "Сьеро", "Овьедо"]
        );
        let text = one(r);
        let spain = text.find("🇪🇸 Испания").unwrap();
        let asturias = text.find("<b>Астурия</b>").unwrap();
        assert!(spain < asturias && text.find("Сьеро").unwrap() > asturias);
    }

    #[test]
    fn empty_section_is_hidden() {
        let mut c = cfg();
        c.sections.push(section("asturias", "Астурия", 3));
        let text = one(render(&[entry(4, "Мадрид")], morning(), &c));
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
