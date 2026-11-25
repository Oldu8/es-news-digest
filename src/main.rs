//! CLI entry point. `digest run [--dry-run]`, `digest serve`, `digest collect`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};
use tracing::{error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder as RollingBuilder, Rotation};
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

use es_news_digest::config::{Config, Secrets};
use es_news_digest::{pipeline, schedule, telegram::Telegram, text::truncate_chars};

#[derive(Parser)]
#[command(
    name = "digest",
    version,
    about = "Daily Spanish news digest in Russian for Telegram"
)]
struct Cli {
    /// Path to config.yaml
    #[arg(
        short,
        long,
        default_value = "config.yaml",
        env = "DIGEST_CONFIG",
        global = true
    )]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the pipeline once.
    Run {
        /// Print the digest to stdout; do not post and do not touch SQLite.
        #[arg(long)]
        dry_run: bool,
    },
    /// Stay alive and run every day at the configured time.
    Serve,
    /// Debug helper: fetch sources and print deduplicated candidates (no LLM).
    Collect,
}

#[tokio::main]
async fn main() -> Result<()> {
    // .env is optional: in Docker the variables come from env_file.
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    let _guard = init_logging(&cfg.paths.log_dir)?;
    let secrets = Secrets::from_env();
    info!(?secrets, config = %cli.config.display(), "starting");

    match cli.cmd {
        Cmd::Run { dry_run } => {
            let res = pipeline::run(&cfg, &secrets, dry_run).await;
            match res {
                Ok(text) if dry_run => println!("{text}"),
                Ok(_) => {}
                Err(e) => {
                    error!("run failed: {e:#}");
                    if !dry_run {
                        alert_admin(&secrets, &format!("прогон упал:\n{e:#}")).await;
                    }
                    return Err(e);
                }
            }
        }
        Cmd::Serve => serve(cli.config, cfg, secrets).await?,
        Cmd::Collect => {
            let http = pipeline::http_client(&cfg)?;
            let now = Utc::now();
            let articles = pipeline::collect(&cfg, &http, now).await?;
            for a in pipeline::prepare(articles, &cfg, &[], now) {
                println!(
                    "{} | {} | {} | {}\n    {}",
                    a.published.format("%m-%d %H:%M"),
                    a.source,
                    a.section.as_deref().unwrap_or("-"),
                    a.title,
                    a.url
                );
            }
        }
    }
    Ok(())
}

/// Console (stderr, so `--dry-run` stdout stays clean) + daily-rotated file.
fn init_logging(dir: &Path) -> Result<WorkerGuard> {
    std::fs::create_dir_all(dir)?;
    let appender = RollingBuilder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("digest")
        .filename_suffix("log")
        .max_log_files(14)
        .build(dir)?;
    let (file_writer, guard) = tracing_appender::non_blocking(appender);
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(filter()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer)
                .with_filter(filter()),
        )
        .init();
    Ok(guard)
}

/// Short plain-text error message to the admin's private chat. Never fails:
/// if even this does not work, it is only logged.
async fn alert_admin(secrets: &Secrets, msg: &str) {
    let (Some(token), Some(chat)) = (&secrets.telegram_bot_token, &secrets.telegram_admin_chat_id)
    else {
        warn!("cannot alert admin: TELEGRAM_BOT_TOKEN or TELEGRAM_ADMIN_CHAT_ID missing");
        return;
    };
    let text = format!("⚠️ es-news-digest: {}", truncate_chars(msg, 1500));
    let res = match Telegram::new(token.clone()) {
        Ok(tg) => tg.send(chat, &text, false).await,
        Err(e) => Err(e),
    };
    if let Err(e) = res {
        error!("admin alert failed: {e:#}");
    }
}

/// Sleep until the next scheduled time, run, repeat. The config is re-read
/// before every run, so edits to config.yaml apply without a restart.
async fn serve(path: PathBuf, mut cfg: Config, secrets: Secrets) -> Result<()> {
    loop {
        let next = schedule::next_run(Utc::now(), cfg.schedule.time()?, cfg.schedule.tz()?);
        info!(next_run = %next.with_timezone(&cfg.schedule.tz()?), "waiting");
        let wait = (next - Utc::now()).to_std().unwrap_or_default();

        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown_signal() => {
                info!("shutdown signal received");
                return Ok(());
            }
        }

        match Config::load(&path) {
            Ok(new) => cfg = new,
            Err(e) => {
                warn!("config reload failed, keeping the previous one: {e:#}");
                alert_admin(
                    &secrets,
                    &format!("config.yaml не читается, использую старый: {e:#}"),
                )
                .await;
            }
        }
        if !run_with_retries(&cfg, &secrets).await {
            return Ok(());
        }
    }
}

/// One scheduled run with `schedule.retry`: a failed attempt is repeated after
/// a pause (Gemini 503 "high demand" usually passes in minutes). The admin is
/// alerted only when the last attempt fails, or right away if the digest was
/// already posted (a retry would post it twice). Returns false on shutdown.
async fn run_with_retries(cfg: &Config, secrets: &Secrets) -> bool {
    let retry = &cfg.schedule.retry;
    for attempt in 1..=retry.attempts {
        let e = match pipeline::run(cfg, secrets, false).await {
            Ok(_) => return true,
            Err(e) => e,
        };
        let last =
            attempt == retry.attempts || e.downcast_ref::<pipeline::AlreadyPosted>().is_some();
        if last {
            error!(attempt, "run failed: {e:#}");
            let tries = if attempt > 1 {
                format!(" (попыток: {attempt})")
            } else {
                String::new()
            };
            alert_admin(secrets, &format!("прогон упал{tries}:\n{e:#}")).await;
            return true;
        }
        warn!(
            attempt,
            retry_in_min = retry.delay_minutes,
            "run failed, will retry: {e:#}"
        );
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(retry.delay_minutes * 60)) => {}
            _ = shutdown_signal() => {
                info!("shutdown signal received");
                return false;
            }
        }
    }
    true
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
