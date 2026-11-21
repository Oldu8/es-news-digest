//! Small string helpers shared by the parsers and the renderer.

use scraper::Html;

/// Collapse runs of whitespace (including newlines) into single spaces and trim.
pub fn squash_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip HTML tags from a feed description and squash whitespace.
pub fn strip_html(s: &str) -> String {
    let fragment = Html::parse_fragment(s);
    let text: Vec<&str> = fragment.root_element().text().collect();
    squash_ws(&text.join(" "))
}

/// Cut a string to at most `max` characters on a word boundary, adding "…".
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    let cut = match cut.rfind(' ') {
        Some(i) if i > cut.len() / 2 => &cut[..i],
        _ => cut.as_str(),
    };
    format!("{}…", cut.trim_end_matches([',', '.', ';', ':', ' ']))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squashes_newlines_and_spaces() {
        assert_eq!(squash_ws("\n   Hola \n\t mundo  "), "Hola mundo");
    }

    #[test]
    fn strips_tags() {
        assert_eq!(
            strip_html("<p><img src=x>Uno</p><div><p class=a>dos &amp; tres</p></div>"),
            "Uno dos & tres"
        );
    }

    #[test]
    fn truncates_on_word_boundary() {
        assert_eq!(truncate_chars("corto", 10), "corto");
        assert_eq!(truncate_chars("una frase bastante larga", 12), "una frase…");
    }
}
