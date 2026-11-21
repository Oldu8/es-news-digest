# es-news-digest

Daily digest of Spanish news (Asturias, autónomo taxes, housing, documents for
Ukrainians, prices) in Russian, posted to a Telegram channel at 08:40
Europe/Madrid. Written in Rust.

**How it works:** sources (`src/sources/`: Spanified HTML, RSS, Google News
sitemap) are fetched concurrently → last 24 h only → dedup by normalized URL and
similar titles, minus anything already posted (SQLite, 30 days) → one Gemini
call with a JSON `responseSchema` filters, scores 1–5, merges duplicates and
translates → HTML message (≤ 4096 chars) → Bot API `sendMessage`. Errors go to
your private chat with the bot.

## Setup

```bash
cp .env.example .env   # fill GEMINI_API_KEY, TELEGRAM_* (see below)
cargo run -- run --dry-run   # prints the digest, posts nothing, writes nothing
```

Telegram ids: write `/start` to the bot in a private chat and post anything in
the channel (the bot is an admin there), then open
`https://api.telegram.org/bot<TOKEN>/getUpdates`. Your chat id is
`message.chat.id`; the channel id is `channel_post.chat.id` (starts with `-100`).

## Commands

- `digest run` — one run: collect, summarize, post, remember.
- `digest run --dry-run` — print to stdout only.
- `digest serve` — stay alive and run daily at `schedule.time`.
- `digest collect` — debug: list deduplicated candidates without the LLM.

`--config path` (or `DIGEST_CONFIG`) picks another config. Logs: stderr plus
daily-rotated files in `paths.log_dir` (14 kept). `RUST_LOG=debug` for more.

## Running

- **Docker:** `docker compose up -d --build` (runs `serve`, 128 MB limit,
  `./data` and `./logs` volumes, `config.yaml` mounted read-only).
- **systemd timer / cron:** build with `cargo build --release` and call
  `digest run` once a day, e.g. cron `40 8 * * * cd /opt/es-news-digest && ./digest run`
  with `CRON_TZ=Europe/Madrid`.

## Changing topics and sources

Everything is in `config.yaml`, no rebuild needed (`serve` re-reads it before
each run):

- `topics.keep` / `topics.drop` — plain-language rules for Gemini.
- `sources` — `kind: rss` (list of `urls`), `kind: sitemap` (`url` +
  `sections`, the first path segment of article URLs, e.g. `oviedo`),
  `kind: spanified`. Order matters: on duplicates the earlier source wins.
- `gemini.model`, `digest.max_items`, `digest.min_importance`,
  `dedup.title_similarity`, `schedule.time`/`timezone`.

Tests run on saved pages in `tests/fixtures/`: `cargo test`.
