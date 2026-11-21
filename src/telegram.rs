//! Minimal Telegram Bot API client: just `sendMessage`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub struct Telegram {
    http: reqwest::Client,
    token: String,
}

impl Telegram {
    pub fn new(token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, token })
    }

    /// Send a message. `html = true` uses `parse_mode: HTML`; link previews
    /// are disabled so the digest stays compact.
    pub async fn send(&self, chat_id: &str, text: &str, html: bool) -> Result<()> {
        let mut body = json!({
            "chat_id": chat_id,
            "text": text,
            "link_preview_options": {"is_disabled": true},
        });
        if html {
            body["parse_mode"] = json!("HTML");
        }
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        // Do not let reqwest put the URL (with the token) into the error text.
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Telegram request failed: {}", e.without_url()))?;
        let status = resp.status();
        let v: Value = resp.json().await.context("Telegram response is not JSON")?;
        if !v["ok"].as_bool().unwrap_or(false) {
            bail!(
                "Telegram sendMessage HTTP {status}: {}",
                v["description"].as_str().unwrap_or("unknown error")
            );
        }
        Ok(())
    }
}
