//! Gemini REST client (`models/{model}:generateContent`) with structured output:
//! the response is forced into a JSON schema, so we can deserialize it straight
//! into Rust structs.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, info};

use crate::article::Article;
use crate::config::{GeminiConfig, Lang, SectionConfig, TopicsConfig};
use crate::text::truncate_chars;

/// One digest entry as returned by the model.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct LlmItem {
    /// Ids of the input articles describing this story (several = duplicates).
    pub ids: Vec<usize>,
    /// Key of the digest section (`digest.sections[].key`).
    pub section: String,
    /// Keys from `topics.catalog`, most relevant first.
    pub topics: Vec<String>,
    /// 1 (marginal) … 5 (must know today).
    pub importance: u8,
    /// Headline in the digest language.
    pub title: String,
    /// Summary in the digest language.
    pub summary: String,
    /// Index in the ALREADY PUBLISHED list if this is the same story with
    /// nothing really new, else -1. Such items are dropped in code.
    pub repeat_of: i64,
}

#[derive(Debug, Deserialize)]
struct LlmOutput {
    items: Vec<LlmItem>,
}

/// Parse the model's JSON (`{"items": [...]}`) and clamp importance to 1..=5.
/// Shared with the OpenAI fallback, which answers in the same shape.
pub fn parse_items(json: &str) -> serde_json::Result<Vec<LlmItem>> {
    let out: LlmOutput = serde_json::from_str(json)?;
    Ok(out
        .items
        .into_iter()
        .map(|mut it| {
            it.importance = it.importance.clamp(1, 5);
            it
        })
        .collect())
}

pub struct Gemini {
    http: reqwest::Client,
    cfg: GeminiConfig,
    api_key: String,
}

/// System prompt shared by all providers.
pub fn system_prompt(lang: Lang) -> String {
    format!(
        "You are the editor of a daily digest of Spanish news for Ukrainians living in Spain: \
         people with temporary protection or other residence status, working as employees or \
         autónomos, renting a home, often with children at school. You get the news of the \
         last day in Spanish or English. Pick only what is practically useful to these readers, \
         merge duplicates and retell each story in {lang}, briefly and precisely, without filler \
         or opinions. Do not invent facts that are not in the headline or the lead. Keep numbers, \
         dates, amounts and percentages.",
        lang = lang.prompt_name()
    )
}

/// JSON schema for `generationConfig.responseSchema` (OpenAPI subset).
/// `section` and `topics` are enums of the configured keys, so the model
/// cannot invent one.
pub fn response_schema(sections: &[SectionConfig], topics: &TopicsConfig, lang: Lang) -> Value {
    let keys: Vec<&str> = sections.iter().map(|s| s.key.as_str()).collect();
    let topic_keys: Vec<&str> = topics.catalog.iter().map(|t| t.key.as_str()).collect();
    let lang = lang.prompt_name();
    json!({
        "type": "OBJECT",
        "properties": {
            "items": {
                "type": "ARRAY",
                "items": {
                    "type": "OBJECT",
                    "properties": {
                        "ids": {"type": "ARRAY", "items": {"type": "INTEGER"},
                                "description": "ids of all input articles about this story"},
                        "section": {"type": "STRING", "format": "enum", "enum": keys,
                                    "description": "digest section"},
                        "topics": {"type": "ARRAY",
                                   "items": {"type": "STRING", "format": "enum", "enum": topic_keys},
                                   "description": "1-3 topic keys, most relevant first"},
                        "importance": {"type": "INTEGER", "minimum": 1, "maximum": 5},
                        "title": {"type": "STRING", "description": format!("headline in {lang}, up to 90 characters")},
                        "summary": {"type": "STRING", "description": format!("the gist in 2-4 sentences in {lang}, up to 500 characters")},
                        "repeat_of": {"type": "INTEGER", "description": "number in ALREADY PUBLISHED if this story was already published with nothing new, else -1"}
                    },
                    "required": ["ids", "section", "topics", "importance", "title", "summary", "repeat_of"],
                    "propertyOrdering": ["ids", "repeat_of", "section", "topics", "importance", "title", "summary"]
                }
            }
        },
        "required": ["items"]
    })
}

/// The user prompt: topic rules plus one compact line per article.
pub fn build_prompt(
    articles: &[Article],
    topics: &TopicsConfig,
    sections: &[SectionConfig],
    lang: Lang,
    published: &[String],
) -> String {
    let mut p = String::new();
    p.push_str("TOPICS we cover (field `topics`), keep only stories that fit one of them:\n");
    for t in &topics.catalog {
        p.push_str(&format!("- {}: {}\n", t.key, t.description));
    }
    p.push_str("\nDROP:\n");
    for t in &topics.drop {
        p.push_str(&format!("- {t}\n"));
    }
    p.push_str("\nSECTIONS (field `section`):\n");
    for sec in sections {
        p.push_str(&format!("- {}: {}\n", sec.key, sec.description));
    }
    if !published.is_empty() {
        p.push_str(
            "\nALREADY PUBLISHED in the last digests (number | headline). For every item set \
             `repeat_of`: the number of the published headline about the same story (also from \
             another source or with other wording) when the articles bring nothing really new; \
             -1 when the story is new or has a real new development (a decision taken, new \
             figures, a new date; then say in the title what is new):\n",
        );
        for (i, t) in published.iter().enumerate() {
            p.push_str(&format!("{i} | {t}\n"));
        }
    }
    let max_items: usize = sections.iter().map(|s| s.max_items).sum();
    p.push_str(&format!(
        "\nRules:\n\
         - If several articles cover the same story (also from different sources or in \
           different languages), return ONE item and list all their ids in `ids`. One item \
           = one event. Never merge different events into one item, even when they share a \
           town or a topic (measles and an egg recall are two items; inflation and the stock \
           market are two items). A title must name one concrete event; a generic title like \
           \"News of Asturias\" or \"Infrastructure news\" means you merged events, split them.\n\
         - topics: 1-3 keys from TOPICS, most relevant first.\n\
         - importance: 5 = directly and urgently affects the readers' money, documents or \
           housing (a new tax, a deadline, a rule change for Ukrainians, a sharp price rise); \
           4 = noticeable effect; 3 = useful to know; 2 = background; 1 = hardly matters.\n\
         - Keep news about other regions of Spain only if it concerns the whole country.\n\
         - Put every item into exactly one section. News from Asturias with consequences for \
           the whole country goes to the general section.\n\
         - Return ALL stories that fit TOPICS, even non-urgent ones (importance 2-3). Do not \
           drop stories for brevity: the count depends only on the day. Return an empty list \
           only if nothing fits. At most {} items.\n\
         - summary: 2-4 sentences, up to 500 characters. Give the main point and every practical \
           detail from the lead: who is affected, amounts, percentages, dates and deadlines, \
           what to do.\n\
         - Write `title` and `summary` in {}. Keep Spanish terms and abbreviations as they \
           are, in Latin letters (IVA, IRPF, IPC, NIE, TIE, SEPE, DGT, cuota, cita previa).\n\n\
         NEWS (id | source | section | headline | lead):\n",
        max_items * 2,
        lang.prompt_name()
    ));
    for (i, a) in articles.iter().enumerate() {
        let summary = a
            .summary
            .as_deref()
            .map(|s| truncate_chars(s, 600))
            .unwrap_or_default();
        p.push_str(&format!(
            "{i} | {} | {} | {} | {}\n",
            a.source,
            a.section.as_deref().unwrap_or("-"),
            a.title,
            summary
        ));
    }
    p
}

impl Gemini {
    pub fn new(cfg: GeminiConfig, api_key: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build()?;
        Ok(Self { http, cfg, api_key })
    }

    pub async fn select(
        &self,
        articles: &[Article],
        topics: &TopicsConfig,
        sections: &[SectionConfig],
        lang: Lang,
        published: &[String],
    ) -> Result<Vec<LlmItem>> {
        let prompt = build_prompt(articles, topics, sections, lang, published);
        let mut generation = json!({
            "responseMimeType": "application/json",
            "responseSchema": response_schema(sections, topics, lang),
        });
        if let Some(t) = self.cfg.temperature {
            generation["temperature"] = json!(t);
        }
        let body = json!({
            "systemInstruction": {"parts": [{"text": system_prompt(lang)}]},
            "contents": [{"role": "user", "parts": [{"text": prompt}]}],
            "generationConfig": generation,
        });

        let url = format!(
            "{}/models/{}:generateContent",
            self.cfg.base_url.trim_end_matches('/'),
            self.cfg.model
        );
        info!(model = %self.cfg.model, articles = articles.len(), prompt_chars = prompt.len(), "asking Gemini");
        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .context("Gemini request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("reading Gemini response")?;
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| truncate_chars(&text, 300));
            bail!("Gemini HTTP {status}: {msg}");
        }
        let items = parse_response(&text)?;
        debug!(?items, "Gemini items");
        Ok(items)
    }
}

/// Extract and validate the JSON payload from a `generateContent` response.
pub fn parse_response(raw: &str) -> Result<Vec<LlmItem>> {
    let v: Value = serde_json::from_str(raw).context("Gemini response is not JSON")?;
    if let Some(u) = v.get("usageMetadata") {
        info!(
            prompt_tokens = u["promptTokenCount"].as_u64(),
            output_tokens = u["candidatesTokenCount"].as_u64(),
            "Gemini usage"
        );
    }
    let cand = v["candidates"].get(0).ok_or_else(|| {
        let reason = v["promptFeedback"]["blockReason"]
            .as_str()
            .unwrap_or("none");
        anyhow!("Gemini returned no candidates (block reason: {reason})")
    })?;
    let finish = cand["finishReason"].as_str().unwrap_or("");
    let text: String = cand["content"]["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|p| !p["thought"].as_bool().unwrap_or(false))
                .filter_map(|p| p["text"].as_str())
                .collect()
        })
        .unwrap_or_default();
    if text.trim().is_empty() {
        bail!("Gemini returned an empty answer (finishReason: {finish})");
    }
    parse_items(&text).with_context(|| {
        format!(
            "Gemini JSON does not match the schema (finishReason: {finish}): {}",
            truncate_chars(&text, 200)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TopicConfig;
    use chrono::Utc;

    #[test]
    fn parses_structured_answer() {
        let raw = r#"{
          "candidates": [{
            "content": {"parts": [{"text": "{\"items\":[{\"ids\":[0,3],\"section\":\"spain\",\"topics\":[\"taxes\"],\"importance\":7,\"title\":\"Т\",\"summary\":\"С\",\"repeat_of\":-1}]}"}]},
            "finishReason": "STOP"
          }],
          "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 20}
        }"#;
        let items = parse_response(raw).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].ids, [0, 3]);
        assert_eq!(items[0].section, "spain");
        assert_eq!(items[0].importance, 5, "clamped");
        assert_eq!(items[0].topics, ["taxes"]);
    }

    #[test]
    fn reports_blocked_prompt() {
        let err = parse_response(r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#).unwrap_err();
        assert!(err.to_string().contains("SAFETY"));
    }

    #[test]
    fn prompt_lists_articles_with_ids() {
        let a = Article {
            source: "El Comercio".into(),
            title: "Sube la cuota de autónomos".into(),
            summary: Some("A partir de enero".into()),
            url: "https://x.es".into(),
            published: Utc::now(),
            section: Some("asturias".into()),
        };
        let topics = TopicsConfig {
            catalog: vec![TopicConfig {
                key: "taxes".into(),
                title: "Податки".into(),
                tag: "податки".into(),
                description: "autónomos and taxes".into(),
            }],
            drop: vec!["sport".into()],
        };
        let sections = vec![SectionConfig {
            key: "asturias".into(),
            title: "Астурия".into(),
            description: "новости Астурии".into(),
            max_items: 3,
        }];
        let p = build_prompt(&[a.clone(), a], &topics, &sections, Lang::Uk, &[]);
        assert!(!p.contains("ALREADY PUBLISHED"));
        let with_memory =
            build_prompt(&[], &topics, &sections, Lang::Uk, &["Інфляція 4,1%".into()]);
        assert!(with_memory.contains("ALREADY PUBLISHED"));
        assert!(with_memory.contains("\n0 | Інфляція 4,1%\n"));
        assert!(p.contains("- asturias: новости Астурии"));
        assert!(p.contains("- taxes: autónomos and taxes"));
        assert!(p.contains("At most 6 items"));
        assert!(p.contains("in Ukrainian"));
        assert!(p.contains(
            "0 | El Comercio | asturias | Sube la cuota de autónomos | A partir de enero"
        ));
        assert!(p.contains("\n1 | El Comercio"));
        assert!(p.contains("- sport"));
        let schema = response_schema(&sections, &topics, Lang::Uk);
        let item = &schema["properties"]["items"]["items"]["properties"];
        assert_eq!(item["topics"]["items"]["enum"], json!(["taxes"]));
    }
}
