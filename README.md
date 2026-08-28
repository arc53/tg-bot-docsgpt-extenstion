# Telegram DocsGPT extension

Telegram bots for your [DocsGPT](https://www.docsgpt.cloud/) agents. One small binary runs any number of bots, each connected to one or more agents, with the answer experience Telegram now supports for AI bots: live streamed drafts with a Stop button, rich formatted answers (headings, tables, code, math), collapsible sources, file and photo input, voice notes, and files that your agent's tools produce sent back into the chat.

Version 2 is a rewrite in Rust. The Python bot lives on the [`legacy-python`](https://github.com/arc53/tg-bot-docsgpt-extenstion/tree/legacy-python) branch and the `:1` image tag; existing `.env` files keep working unchanged.

## Features

- **Streaming answers** — private chats show the answer as it is generated, with Telegram's own Stop button; groups get a typing indicator and the final message.
- **Rich messages** — the agent's markdown is rendered natively: headings, lists, tables, code blocks, LaTeX, quotes. Sources are tucked into a collapsible block. Falls back to MarkdownV2, then plain text, if a message can't be rendered.
- **Multi-turn memory** — every chat (and every forum topic) keeps its own conversation with each agent; `/new` starts over.
- **Files in** — photos and documents are uploaded to DocsGPT as attachments; voice notes and audio are transcribed and asked as questions (optionally answered with a voice note).
- **Files out** — when the agent runs tools (code execution, document or image generation) the resulting files and images are downloaded and sent to the chat.
- **Many bots, many agents** — a TOML file declares bots and their agents. Users pick an agent with `/agents`, `/agent <name>` or a one-off `#name` prefix.
- **Groups done properly** — answers only when mentioned or replied to (configurable), one conversation per forum topic, private ("ephemeral") replies for `/help` and `/agents` so the group isn't cluttered.
- **Telegram Business** — answer customers inside a connected business account's chats.
- **Guest mode** — answer when mentioned in chats the bot is not a member of.
- **Inline mode** — `@yourbot question?` from any chat.
- **Reactions** — 👀 while working; thumbs up/down on answers are logged as feedback.
- **Polling or webhooks**, `/healthz`, structured logs, SQLite by default (MongoDB or in-memory optional), a small distroless container image for amd64 and arm64.

## Quick start

You need a bot token from [@BotFather](https://t.me/BotFather) and an agent API key from DocsGPT (Agents → your agent → API key).

### Docker

```bash
git clone https://github.com/arc53/tg-bot-docsgpt-extenstion.git
cd tg-bot-docsgpt-extenstion
cp .env.example .env      # fill in TELEGRAM_BOT_TOKEN and API_KEY
docker compose up -d
```

Or without compose:

```bash
docker run -d --name docsgpt-tg --env-file .env -v botdata:/app/data arc53/tg-bot-docsgpt-extenstion:latest
```

### Binary

```bash
cargo build --release
./target/release/docsgpt-telegram --check     # validates config and tokens
./target/release/docsgpt-telegram
```

Rust 1.85 or newer is required to build.

## Configuration

### One bot: environment variables

Same variables as version 1. Put them in `.env` next to the binary or pass them to the container.

| Variable | Purpose |
|---|---|
| `TELEGRAM_BOT_TOKEN` | Bot token from BotFather. Required. |
| `API_KEY` | DocsGPT agent API key for the default agent. |
| `API_KEY_<NAME>` | Additional agents, addressable as `#name` or via `/agent name`. |
| `API_BASE` | DocsGPT server URL (default `https://gptcloud.arc53.com`). |
| `SQLITE_PATH` | SQLite file (default `data/docsgpt-telegram.db`; `/app/data/…` in Docker). |
| `STORAGE_TYPE` | `mongodb` or `memory` to override SQLite. With MongoDB: `MONGODB_URI`, `MONGODB_DB_NAME`, `MONGODB_COLLECTION_NAME` (the v1 collection, migrated on first use). |
| `GROUPS_MODE` | `mention` (default), `all`, or `off`. |
| `STREAMING` | `false` to disable live drafts. |
| `VOICE_REPLIES` | `true` to answer voice notes with a voice note. |
| `WELCOME_TEXT` | Text for `/start`; `{name}` and `{agents}` are expanded. |
| `MENU_BUTTON_URL` | Opens a Mini App (for example the DocsGPT web widget) from the menu button. |
| `WEBHOOK_PUBLIC_URL`, `WEBHOOK_SECRET`, `HTTP_BIND` | Webhook mode (see below). |
| `RUST_LOG`, `LOG_FORMAT=json` | Logging. |

### Many bots: `docsgpt-tg.toml`

Create `docsgpt-tg.toml` in the working directory (or set `DOCSGPT_TG_CONFIG=/path/to/file`). `${VAR}` references are replaced from the environment, so secrets can stay in `.env`. See [`docsgpt-tg.example.toml`](docsgpt-tg.example.toml) for every option.

```toml
[[bots]]
name = "support"
token = "${TG_TOKEN_SUPPORT}"

  [[bots.agents]]
  name = "support"
  api_key = "${DOCSGPT_KEY_SUPPORT}"
  description = "Product and billing questions"
  default = true

  [[bots.agents]]
  name = "sales"
  api_key = "${DOCSGPT_KEY_SALES}"
  description = "Pricing and plans"

[[bots]]
name = "internal-docs"
token = "${TG_TOKEN_INTERNAL}"
allowed_chats = [-1001234567890]

  [[bots.agents]]
  name = "docs"
  api_key = "${DOCSGPT_KEY_DOCS}"
```

Per-bot options: `mode` (`polling`/`webhook`), `groups` (`mention`/`all`/`off`), `streaming`, `reactions`, `attachments`, `voice_replies`, `business`, `guest`, `inline`, `allowed_chats`, `welcome`, `description`, `short_description`, `menu_button_url`, `max_file_mb`, `api_base`.

In Docker, mount the file: `-v ./docsgpt-tg.toml:/app/docsgpt-tg.toml:ro`.

### Storage

DocsGPT keeps the conversation transcript; the bot only stores which conversation each chat is in, the active agent, and business-connection details.

- `sqlite` (default) — a single file, kept in the `/app/data` volume in Docker.
- `mongodb` — for shared deployments, or if you already ran version 1: conversations stored by the Python bot are picked up the first time a chat writes again.
- `memory` — lost on restart; fine for trying things out.

### Webhook mode

Set `mode = "webhook"` on a bot (or `WEBHOOK_PUBLIC_URL` in the single-bot layout) and `server.public_url`. The bot registers `<public_url>/webhook/<bot name>` with a secret token and serves all webhook bots and `GET /healthz` on `server.bind` (default `0.0.0.0:8080`). Polling needs no inbound connectivity and is the default.

## Setting up the bot in Telegram

All of these are toggled in [@BotFather](https://t.me/BotFather) under *Bot Settings*:

- **Groups** — with Group Privacy on (the default) the bot only sees mentions and replies, which is exactly what `groups = "mention"` needs. Turn privacy off only for `groups = "all"`.
- **Inline mode** — enable it for `@yourbot question?`.
- **Business mode** — enable it so business accounts can connect the bot; the bot answers messages from customers (never the account owner's own messages) once the connection grants *reply* rights.
- **Guest mode** — enable it so the bot can be mentioned in chats it hasn't joined.
- **Topics** — enabling topics for the bot gives each private-chat topic its own conversation.

The bot sets its command menu, description and menu button itself at startup (from your config).

## Using the bot

- Send a question. In private chats you'll see the answer stream in; press **Stop** to cut it short.
- Send a **photo or document** (with an optional caption as the question) — it's uploaded to DocsGPT and the agent answers about it. Albums are handled as one question.
- Send a **voice note** — it's transcribed and answered.
- `/new` starts a fresh conversation with the current agent. `/agents` shows a picker, `/agent sales` switches, `#sales what's the price?` asks one agent just once.
- In groups: mention `@yourbot` or reply to one of its messages. Each forum topic keeps its own conversation.
- Inline: type `@yourbot how do I reset my password?` in any chat and pick the answer.

## Upgrading from version 1

- The `:1` image tag and the `legacy-python` branch keep the Python bot; `:latest` and `:2` are the Rust bot.
- Your `.env` works as is. `STORAGE_TYPE=mongodb` deployments keep their MongoDB and migrate conversations lazily; deployments without `STORAGE_TYPE` now persist to SQLite instead of memory.
- Answers now stream and render as rich messages; the `#agent` prefix still works and `/agents` is the new way to switch.

## Development

```bash
cargo test                                     # unit + mock-server end-to-end tests
DOCSGPT_LIVE_KEY=<agent key> cargo test --test docsgpt_live -- --ignored --nocapture   # against a real DocsGPT
TELEGRAM_API_URL=http://localhost:8081 ...     # point the bot at a local Bot API server or a mock
TG_RATE_LIMITS=off ...                         # disable outbound pacing (tests only)
```

Layout: `src/config.rs` (TOML + env), `src/storage/` (memory, SQLite, MongoDB), `src/docsgpt/` (streaming client, attachments, speech, artifacts), `src/telegram/` (API wrapper, rendering, rate limits, polling/webhooks, raw params for the newest Bot API fields), `src/handlers/` (messages, commands, attachments, business, guest, inline, callbacks).

## License

MIT — see [LICENSE](LICENSE).

## Acknowledgments

- [DocsGPT](https://github.com/arc53/DocsGPT)
- [Telegram Bot API](https://core.telegram.org/bots/api)
- [frankenstein](https://github.com/ayrat555/frankenstein)
