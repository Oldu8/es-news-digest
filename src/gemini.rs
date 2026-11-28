//! Gemini REST client (`models/{model}:generateContent`) with structured output:
//! the response is forced into a JSON schema, so we can deserialize it straight
//! into Rust structs.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, info};

use crate::article::Article;
use crate::config::{GeminiConfig, SectionConfig, TopicsConfig};
use crate::text::truncate_chars;

/// One digest entry as returned by the model.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct LlmItem {
    /// Ids of the input articles describing this story (several = duplicates).
    pub ids: Vec<usize>,
    /// Key of the digest section (`digest.sections[].key`).
    pub section: String,
    pub topic: String,
    /// 1 (marginal) … 5 (must know today).
    pub importance: u8,
    pub title_ru: String,
    pub summary_ru: String,
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
pub const SYSTEM: &str = "Ты редактор ежедневной сводки испанских новостей для русскоязычного \
жителя Астурии (живёт около Овьедо/Сьеро), который работает autónomo и является \
украинцем с protección temporal. Тебе дают список новостей за последние сутки на испанском \
или английском. Твоя задача: выбрать только то, что практически важно этому читателю, \
объединить дубли и пересказать по-русски коротко и точно, без воды и оценок. \
Не выдумывай факты, которых нет в заголовке или лиде. Числа, даты, суммы и проценты сохраняй.";

/// JSON schema for `generationConfig.responseSchema` (OpenAPI subset).
/// `section` is an enum of the configured keys, so the model cannot invent one.
pub fn response_schema(sections: &[SectionConfig]) -> Value {
    let keys: Vec<&str> = sections.iter().map(|s| s.key.as_str()).collect();
    json!({
        "type": "OBJECT",
        "properties": {
            "items": {
                "type": "ARRAY",
                "items": {
                    "type": "OBJECT",
                    "properties": {
                        "ids": {"type": "ARRAY", "items": {"type": "INTEGER"},
                                "description": "id всех входных новостей об этом сюжете"},
                        "section": {"type": "STRING", "format": "enum", "enum": keys,
                                    "description": "раздел сводки"},
                        "topic": {"type": "STRING", "description": "к какой теме из списка относится"},
                        "importance": {"type": "INTEGER", "minimum": 1, "maximum": 5},
                        "title_ru": {"type": "STRING", "description": "заголовок по-русски, до 90 символов"},
                        "summary_ru": {"type": "STRING", "description": "суть в 2–4 предложениях по-русски, до 500 символов"}
                    },
                    "required": ["ids", "section", "topic", "importance", "title_ru", "summary_ru"],
                    "propertyOrdering": ["ids", "section", "topic", "importance", "title_ru", "summary_ru"]
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
) -> String {
    let mut p = String::new();
    p.push_str("ОСТАВЛЯЕМ только темы:\n");
    for t in &topics.keep {
        p.push_str(&format!("- {t}\n"));
    }
    p.push_str("\nОТБРАСЫВАЕМ:\n");
    for t in &topics.drop {
        p.push_str(&format!("- {t}\n"));
    }
    p.push_str("\nРАЗДЕЛЫ (поле `section`):\n");
    for sec in sections {
        p.push_str(&format!("- {}: {}\n", sec.key, sec.description));
    }
    let max_items: usize = sections.iter().map(|s| s.max_items).sum();
    p.push_str(&format!(
        "\nПравила:\n\
         - Если несколько новостей про один и тот же сюжет (в том числе из разных источников \
           или на разных языках), верни ОДИН пункт и перечисли все их id в `ids`.\n\
         - importance: 5 — касается денег/документов/жилья читателя напрямую и срочно \
           (новый налог, срок подачи, изменение правил для украинцев, резкий рост цен); \
           4 — заметно влияет; 3 — полезно знать; 2 — фон; 1 — почти не важно.\n\
         - Новость про другие регионы Испании оставляй, только если она касается всей страны.\n\
         - Каждый пункт отнеси ровно к одному разделу. Новость из Астурии с последствиями \
           для всей страны — в общий раздел.\n\
         - Верни ВСЕ новости, которые относятся к темам из списка ОСТАВЛЯЕМ, даже если \
           они не срочные (им importance 2–3). Не отбрасывай новости ради краткости: \
           количество зависит только от дня. Пустой список — только если по темам \
           действительно ничего нет. Максимум {} пунктов.\n\
         - summary_ru: 2–4 предложения, до 500 символов. Передай главное и все практические \
           детали из лида: кого касается, суммы, проценты, даты и сроки, что нужно сделать.\n\
         - Пиши по-русски, испанские термины (IVA, IRPF, NIE, TIE, cuota) оставляй как есть.\n\n\
         НОВОСТИ (id | источник | раздел | заголовок | лид):\n",
        max_items * 2
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
    ) -> Result<Vec<LlmItem>> {
        let prompt = build_prompt(articles, topics, sections);
        let mut generation = json!({
            "responseMimeType": "application/json",
            "responseSchema": response_schema(sections),
        });
        if let Some(t) = self.cfg.temperature {
            generation["temperature"] = json!(t);
        }
        let body = json!({
            "systemInstruction": {"parts": [{"text": SYSTEM}]},
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
    use chrono::Utc;

    #[test]
    fn parses_structured_answer() {
        let raw = r#"{
          "candidates": [{
            "content": {"parts": [{"text": "{\"items\":[{\"ids\":[0,3],\"section\":\"spain\",\"topic\":\"налоги\",\"importance\":7,\"title_ru\":\"Т\",\"summary_ru\":\"С\"}]}"}]},
            "finishReason": "STOP"
          }],
          "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 20}
        }"#;
        let items = parse_response(raw).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].ids, [0, 3]);
        assert_eq!(items[0].section, "spain");
        assert_eq!(items[0].importance, 5, "clamped");
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
            keep: vec!["налоги".into()],
            drop: vec!["спорт".into()],
        };
        let sections = vec![SectionConfig {
            key: "asturias".into(),
            title: "Астурия".into(),
            description: "новости Астурии".into(),
            max_items: 3,
        }];
        let p = build_prompt(&[a.clone(), a], &topics, &sections);
        assert!(p.contains("- asturias: новости Астурии"));
        assert!(p.contains("Максимум 6 пунктов"));
        assert!(p.contains(
            "0 | El Comercio | asturias | Sube la cuota de autónomos | A partir de enero"
        ));
        assert!(p.contains("\n1 | El Comercio"));
        assert!(p.contains("- спорт"));
    }
}
