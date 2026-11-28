# es-news-digest

Daily digest of Spanish news for Ukrainians in Spain (documents, work, taxes,
housing, prices, benefits, schools, health, transport, Asturias) in Ukrainian
(or Russian, `digest.language`), posted to a Telegram channel twice a day.
Written in Rust.

**How it works:** sources (`src/sources/`: Spanified HTML, RSS, Google News
sitemap) are fetched concurrently → last 24 h only → dedup by normalized URL and
similar titles, minus anything already posted (SQLite, 30 days) → one Gemini
call with a JSON `responseSchema` filters, scores 1–5, merges duplicates, tags
each story with topic keys from `topics.catalog` and translates; headlines of
the last `dedup.story_memory_hours` go along, and stories the model marks as
already told (or with a near-identical headline) are dropped → HTML message (≤ 4096 chars) → Bot API `sendMessage`. Errors go to
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
- `digest serve` — stay alive and run daily at `schedule.times` (09:00 and 18:00).
- `digest collect` — debug: list deduplicated candidates without the LLM.

`--config path` (or `DIGEST_CONFIG`) picks another config. Logs: stderr plus
daily-rotated files in `paths.log_dir` (14 kept). `RUST_LOG=debug` for more.

## Running

- **Docker:** `docker compose up -d --build` (runs `serve`, 128 MB limit,
  `./data` and `./logs` volumes, `config.yaml` mounted read-only).
- **systemd timer / cron:** build with `cargo build --release` and call
  `digest run` once a day, e.g. cron `40 8 * * * cd /opt/es-news-digest && ./digest run`
  with `CRON_TZ=Europe/Madrid`.
- **Production (shared VPS):** a checkout in `/opt/es-news-digest` with `.env` next to
  `docker-compose.yml`. `deploy/es-news-digest-autodeploy.timer` runs `deploy/autodeploy.sh`
  every 2 minutes: when `origin/main` moved, it pulls and runs `docker compose up -d --build`.
  So deploying is just `git push`.

## Changing topics and sources

Everything is in `config.yaml`, no rebuild needed (`serve` re-reads it before
each run):

- `digest.language` — `uk` or `ru`: the language the model writes in, plus
  month names and Ранок/Вечір in the title.
- `topics.catalog` — the topics we cover: `key` (stable id; the bot will use it
  for personal topic choice), `title` shown to readers, `description` for the
  model, `tag` the hashtag shown before each headline. Each story gets 1–3
  keys. `topics.drop` — what to throw away.
- The `bonus` section closes the digest with 1–2 pleasant stories (topic
  `good`); its description in `digest.sections` tells the model the rules.
- `sources` — `kind: rss` (list of `urls`), `kind: sitemap` (`url` +
  `sections`, the first path segment of article URLs, e.g. `oviedo`),
  `kind: spanified`. Order matters: on duplicates the earlier source wins.
- `digest.sections` — blocks of the message (now Испания and Астурия): `key`,
  heading, a `description` that tells Gemini what belongs there, `max_items`
  (a safety cap). The length is dynamic: every item with importance >=
  `digest.min_importance` is shown, so quiet days give a short digest.
- `schedule.retry` — a failed run (e.g. Gemini 503) is repeated `attempts`
  times, `delay_minutes` apart; the admin is alerted after the last one.
- `gemini.model`, `digest.min_importance`, `dedup.title_similarity`,
  `schedule.times`/`timezone`.

On the server, change `config.yaml` in the repo and push: autodeploy pulls it
(an edit made on the server itself would block the next `git merge`).

Tests run on saved pages in `tests/fixtures/`: `cargo test`.
