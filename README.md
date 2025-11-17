# RSS-to-Telegram Bot

![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/pandreyn/rss-bot/docker-publish.yml?label=Docker%20build%20and%20push) ![Docker Pulls](https://img.shields.io/docker/pulls/pandreyn/rss-bot) ![Docker Image Size](https://img.shields.io/docker/image-size/pandreyn/rss-bot?label=Docker%20image%20size)

## Description

This app is a **Rust-based RSS-to-Telegram bot**. It monitors multiple RSS/Atom feeds and automatically forwards new entries to a specified Telegram chat. Key features include:

*   **Multi-feed support**
*   **Deduplication** (avoids reposting)
*   **Configurable polling interval**
*   **Persistent state storage**
*   **Structured logging** (human or JSON format)
*   **Docker healthcheck integration**

***

## How It Works

1.  **Configuration**: Reads settings from environment variables (`.env` file).
2.  **Feed Polling**: Fetches and parses feeds at regular intervals.
3.  **Deduplication**: Tracks sent items in a persistent JSON file.
4.  **Telegram Delivery**: Sends new entries to your Telegram chat.
5.  **Logging**: Supports both human-readable and JSON logs.

***

## Installation Steps

### 1. Prerequisites

*   Docker & Docker Compose installed
*   Telegram bot token & chat ID

### 2. Prepare `.env` File

Create a `.env` file in your project directory:

```ini
RSSBOT_TELEGRAM_TOKEN=your_bot_token_here
RSSBOT_TELEGRAM_CHAT_ID=your_chat_id_here
RSSBOT_FEEDS=https://example.com/feed1.xml,https://example.com/feed2.xml
RSSBOT_DEDUP_LIMIT=200
RSSBOT_POLL_EVERY_MINUTES=5
RSSBOT_STATE_FILE=state.json
```

### 3. Build & Run

Place `main.rs` and `docker-compose.yml` in the same directory. Then run:

```sh
docker-compose build
docker-compose up
```
or use following command to run as a daemon:

```sh
docker-compose up -d
```

### 4. Data Persistence

*   State is stored in `./data` (host) mapped to `/app/data` (container).
*   Do not delete the state file unless you want to reset deduplication.

### 5. Monitoring

*   View logs:
    ```sh
    docker-compose logs -f
    ```
*   Healthcheck ensures the bot is running and persisting state.

***

## Example `docker-compose.yml`

```yaml
services:
  rss-bot:
    image: pandreyn/rss-bot:latest
    restart: unless-stopped     # for debuginng it can be set to "no"
    env_file: .env
    volumes:
      - ./data:/app/data
    logging:
      driver: json-file
      options:
        max-size: 10m
        max-file: "5"
    environment:
      # Optional: control log format and verbosity
      - RSSBOT_RUST_LOG=rss_bot=info,reqwest=warn
      # Uncomment for JSON logs (for Loki/ELK/etc)
      # - RSSBOT_RUST_LOG_FORMAT=json
    healthcheck:
      test: ["CMD", "sh", "-c", "test -s /app/state.json || exit 1"]
      interval: 30s
      timeout: 5s
      retries: 3
```

***
