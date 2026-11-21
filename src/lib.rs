//! es-news-digest: collect Spanish news, filter and translate them with Gemini,
//! and post a short Russian digest to a Telegram channel.
//!
//! The binary (`src/main.rs`) is a thin CLI; everything testable lives here.

pub mod article;
pub mod config;
pub mod dedup;
pub mod gemini;
pub mod pipeline;
pub mod render;
pub mod schedule;
pub mod sources;
pub mod store;
pub mod telegram;
pub mod text;
