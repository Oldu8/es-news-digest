//! OpenAI fallback (Chat Completions with Structured Outputs), used when Gemini
//! fails, e.g. 503 "high demand". Same prompt, same answer shape as Gemini, so
//! the rest of the pipeline does not know which model wrote the digest.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tracing::info;

use crate::article::Article;
use crate::config::{Lang, OpenAiConfig, SectionConfig, TopicsConfig};
use crate::gemini::{LlmItem, build_prompt, parse_items, system_prompt};
use crate::text::truncate_chars;

pub struct OpenAi {
    http: reqwest::Client,
    cfg: OpenAiConfig,
    api_key: String,
}

/// Standard JSON Schema in strict mode: every property required, no extras.
pub fn response_schema(sections: &[SectionConfig], topics: &TopicsConfig, lang: Lang) -> Value {
    let keys: Vec<&str> = sections.iter().map(|s| s.key.as_str()).collect();
    let topic_keys: Vec<&str> = topics.catalog.iter().map(|t| t.key.as_str()).collect();
    let lang = lang.prompt_name();
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["items"],
        "properties": {
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["ids", "section", "topics", "importance", "title", "summary", "repeat_of"],
                    "properties": {
                        "ids": {"type": "array", "items": {"type": "integer"},
                                "description": "ids of all input articles about this story"},
                        "section": {"type": "string", "enum": keys, "description": "digest section"},
                        "topics": {"type": "array", "items": {"type": "string", "enum": topic_keys},
                                   "description": "1-3 topic keys, most relevant first"},
                        "importance": {"type": "integer", "description": "1..5"},
                        "title": {"type": "string", "description": format!("headline in {lang}, up to 90 characters")},
                        "summary": {"type": "string", "description": format!("the gist in 2-4 sentences in {lang}, up to 500 characters")},
                        "repeat_of": {"type": "integer", "description": "number in ALREADY PUBLISHED if this story was already published with nothing new, else -1"}
                    }
                }
            }
        }
    })
}

impl OpenAi {
    pub fn new(cfg: OpenAiConfig, api_key: String) -> Result<Self> {
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
        let mut body = json!({
            "model": self.cfg.model,
            "messages": [
                {"role": "system", "content": system_prompt(lang)},
                {"role": "user", "content": prompt},
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": "digest", "strict": true, "schema": response_schema(sections, topics, lang)},
            },
        });
        if let Some(effort) = &self.cfg.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        let url = format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        );
        info!(model = %self.cfg.model, articles = articles.len(), "asking OpenAI");
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.without_url())
            .context("OpenAI request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("reading OpenAI response")?;
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| truncate_chars(&text, 300));
            bail!("OpenAI HTTP {status}: {msg}");
        }
        parse_response(&text)
    }
}

/// Extract the structured answer from a Chat Completions response.
pub fn parse_response(raw: &str) -> Result<Vec<LlmItem>> {
    let v: Value = serde_json::from_str(raw).context("OpenAI response is not JSON")?;
    if let Some(u) = v.get("usage") {
        info!(
            prompt_tokens = u["prompt_tokens"].as_u64(),
            output_tokens = u["completion_tokens"].as_u64(),
            "OpenAI usage"
        );
    }
    let choice = &v["choices"][0];
    let msg = &choice["message"];
    if let Some(refusal) = msg["refusal"].as_str() {
        bail!("OpenAI refused: {}", truncate_chars(refusal, 200));
    }
    let finish = choice["finish_reason"].as_str().unwrap_or("");
    let content = msg["content"]
        .as_str()
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| anyhow!("OpenAI returned an empty answer (finish_reason: {finish})"))?;
    parse_items(content).with_context(|| {
        format!(
            "OpenAI JSON does not match the schema (finish_reason: {finish}): {}",
            truncate_chars(content, 200)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_structured_answer() {
        let raw = r#"{
          "choices": [{
            "message": {"role": "assistant", "refusal": null,
              "content": "{\"items\":[{\"ids\":[2],\"section\":\"asturias\",\"topics\":[\"housing\"],\"importance\":0,\"title\":\"Т\",\"summary\":\"С\",\"repeat_of\":-1}]}"},
            "finish_reason": "stop"
          }],
          "usage": {"prompt_tokens": 100, "completion_tokens": 20}
        }"#;
        let items = parse_response(raw).unwrap();
        assert_eq!(items[0].section, "asturias");
        assert_eq!(items[0].importance, 1, "clamped");
    }

    #[test]
    fn reports_refusal_and_truncation() {
        let refused =
            r#"{"choices":[{"message":{"refusal":"no","content":null},"finish_reason":"stop"}]}"#;
        assert!(
            parse_response(refused)
                .unwrap_err()
                .to_string()
                .contains("refused")
        );
        let cut =
            r#"{"choices":[{"message":{"content":"{\"items\":[{"},"finish_reason":"length"}]}"#;
        assert!(format!("{:#}", parse_response(cut).unwrap_err()).contains("length"));
    }

    #[test]
    fn schema_is_strict_with_key_enums() {
        let sections = vec![SectionConfig {
            key: "spain".into(),
            title: "Испания".into(),
            description: String::new(),
            max_items: 5,
        }];
        let topics = TopicsConfig {
            catalog: vec![crate::config::TopicConfig {
                key: "housing".into(),
                title: "Житло".into(),
                tag: "житло".into(),
                description: String::new(),
            }],
            drop: vec![],
        };
        let s = response_schema(&sections, &topics, Lang::Uk);
        let item = &s["properties"]["items"]["items"];
        assert_eq!(item["additionalProperties"], json!(false));
        assert_eq!(item["properties"]["section"]["enum"], json!(["spain"]));
        assert_eq!(
            item["properties"]["topics"]["items"]["enum"],
            json!(["housing"])
        );
        assert_eq!(
            item["required"].as_array().unwrap().len(),
            item["properties"].as_object().unwrap().len()
        );
    }
}
