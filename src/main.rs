use anyhow::{Context, Result};
use feed_rs::model::{Entry, Feed};
use feed_rs::parser;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, VecDeque},
    env,
    fs,
    io::Cursor,
    path::{Path, PathBuf},
};
use teloxide::{prelude::*, types::ChatId};
use tokio::time::{self, Duration};

/// ---------- Utilities for extracting stable identity & display from an Entry ----------

fn entry_id(entry: &Entry) -> String {
    if let Some(id) = (!entry.id.is_empty()).then(|| entry.id.clone()) {
        return format!("guid:{id}");
    }
    if let (Some(link), Some(published)) = (
        entry
            .links
            .iter()
            .find(|l| l.rel.as_deref().unwrap_or("alternate") == "alternate")
            .map(|l| l.href.clone()),
        entry.published,
    ) {
        return format!("link:{link}:{}", published.timestamp());
    }
    if let (Some(title), Some(published)) = (entry.title.as_ref(), entry.published) {
        // NOTE: depending on feed-rs version, this might be `unix_timestamp()`
        return format!("titlepub:{}{}", title.content, published.timestamp());
    }
    // fallback hash of a few fields
    let mut hasher = Sha1::new();
    hasher.update(
        entry
            .title
            .as_ref()
            .map(|t| t.content.as_str())
            .unwrap_or(""),
    );
    if let Some(href) = entry
        .links
        .iter()
        .find(|l| l.rel.as_deref().unwrap_or("alternate") == "alternate")
        .map(|l| l.href.clone())
    {
        hasher.update("\n");
        hasher.update(href);
    }
    if let (Some(summary), Some(_published)) = (entry.summary.as_ref(), entry.published) {
        hasher.update("\n");
        hasher.update(summary.content.as_str());
    }
    if let Some(content) = &entry.content.as_ref().and_then(|c| c.body.as_deref()) {
        hasher.update("\n");
        hasher.update(content);
    }
    format!("sha1:{:x}", hasher.finalize())
}

fn entry_title(entry: &Entry) -> String {
    entry
        .title
        .as_ref()
        .map(|t| t.content.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "[no title]".into())
}

fn entry_link(entry: &Entry) -> String {
    // prefer "alternate" link, otherwise first link
    if let Some(href) = entry
        .links
        .iter()
        .find(|l| l.rel.as_deref().unwrap_or("alternate") == "alternate")
        .map(|l| l.href.clone())
    {
        return href;
    }
    // FIX: wrap in Some(...) so and_then receives Option<String>
    match entry.links.get(0).and_then(|l| Some(l.href.clone())) {
        Some(href) => href,
        None => "".into(),
    }
}

/// ---------- HTTP fetch ----------

async fn fetch_feed(client: &Client, url: &str) -> Result<Option<Feed>> {
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ERROR fetching {url}: {e}");
            return Err(anyhow::anyhow!("GET {url}: {e}"));
        }
    };

    if resp.status() == StatusCode::NOT_MODIFIED {
        return Ok(None);
    }
    if !resp.status().is_success() {
        anyhow::bail!("{url} -> HTTP {}", resp.status());
    }
    let bytes = resp.bytes().await?;
    let cursor = Cursor::new(bytes);
    let feed = parser::parse(cursor).with_context(|| format!("parse feed {url}"))?;
    Ok(Some(feed))
}

/// ---------- Persistent state (dedup) ----------

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// feed_url -> queue of seen item IDs (oldest at front)
    seen_per_feed: HashMap<String, VecDeque<String>>,
}

impl State {
    fn load(path: &Path) -> Result<Self> {
        if path.exists() {
            let data = fs::read(path).with_context(|| format!("read {}", path.display()))?;
            let s: Self =
                serde_json::from_slice(&data).with_context(|| "parse state JSON".to_string())?;
            Ok(s)
        } else {
            Ok(Default::default())
        }
    }

    fn ensure_feed(&mut self, url: &str) {
        self.seen_per_feed.entry(url.to_string()).or_default();
    }

    fn seen(&self, url: &str, id: &str) -> bool {
        self.seen_per_feed
            .get(url)
            .map_or(false, |dq| dq.contains(&id.to_string()))
    }

    fn mark_sent(&mut self, url: &str, id: String, dedup_limit: usize) {
        let dq = self.seen_per_feed.entry(url.to_string()).or_default();
        if dq.contains(&id) {
            return;
        }
        dq.push_back(id);
        // trim oldest if we exceed limit
        while dq.len() > dedup_limit {
            dq.pop_front();
        }
    }
}

fn save_state_atomic(path: &Path, state: &State) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
    }
    let tmp = path.with_extension("tmp");
    let json =
        serde_json::to_vec_pretty(state).with_context(|| "serialize state JSON".to_string())?;
    fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
    // On Windows, rename will replace if target exists since Rust 1.63+ uses MoveFileEx semantics
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "atomic rename {} -> {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// ---------- Runtime configuration ----------

#[derive(Debug)]
struct Config {
    token: String,
    chat_id: i64,
    feeds: Vec<String>,
    dedup_limit: usize,
    poll_every_minutes: u64,
    state_file: PathBuf,
}

impl Config {
    fn from_env() -> Result<Self> {
        let token = env::var("TELEGRAM_TOKEN")
            .context("TELEGRAM_TOKEN env var is required")?;
        let chat_id: i64 = env::var("TELEGRAM_CHAT_ID")
            .context("TELEGRAM_CHAT_ID env var is required")?
            .parse()
            .context("TELEGRAM_CHAT_ID must be a valid i64")?;

        let feeds_raw = env::var("FEEDS").context("FEEDS env var is required")?;
        let feeds = feeds_raw
            .split(|c: char| c == ',' || c == '\n' || c == ';' || c.is_whitespace())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string())
            .collect::<Vec<_>>();
        if feeds.is_empty() {
            anyhow::bail!("FEEDS must contain at least one URL");
        }

        let dedup_limit: usize = env::var("DEDUP_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200);

        let poll_every_minutes: u64 = env::var("POLL_EVERY_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);

        let state_file = env::var("STATE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("state.json"));

        Ok(Self {
            token,
            chat_id,
            feeds,
            dedup_limit,
            poll_every_minutes,
            state_file,
        })
    }
}

/// ---------- Feed processing ----------

async fn process_feed(
    client: &Client,
    bot: &teloxide::Bot,
    chat_id: ChatId,
    state: &mut State,
    state_path: &Path,
    feed_url: &str,
    dedup_limit: usize,
) -> Result<usize> {
    let feed_opt = fetch_feed(client, feed_url).await?;
    let Some(feed) = feed_opt else {
        return Ok(0);
    };

    let feed_tag = feed
        .title
        .as_ref()
        .map(|t| t.content.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| feed_url.to_string());

    // Send oldest first
    let mut sent_count = 0usize;
    for entry in feed.entries.iter().rev() {
        let id = entry_id(entry);
        if state.seen(feed_url, &id) {
            continue;
        }
        let title = entry_title(entry);
        let link = entry_link(entry);
        let text = format!("[{feed_tag}]\n{title}\n{link}");

        // Send to Telegram
        if let Err(e) = bot.send_message(chat_id, text).await {
            eprintln!("send failed ({feed_url}): {e}");
            continue;
        }

        sent_count += 1;

        // Mark & persist state immediately
        state.mark_sent(feed_url, id, dedup_limit);
        if let Err(e) = save_state_atomic(state_path, state) {
            eprintln!("WARN: save state failed: {e}");
        }

        time::sleep(Duration::from_millis(100)).await;
    }
    Ok(sent_count)
}

async fn run_once(
    client: &Client,
    bot: &teloxide::Bot,
    chat_id: ChatId,
    state: &mut State,
    state_path: &Path,
    feeds: &[String],
    dedup_limit: usize,
) {
    let started = std::time::Instant::now();
    let mut total = 0usize;

    for url in feeds {
        state.ensure_feed(url);
        match process_feed(client, bot, chat_id, state, state_path, url, dedup_limit).await {
            Ok(n) => total += n,
            Err(e) => eprintln!("feed error [{url}]: {e}"),
        }
        // spacing between feeds
        time::sleep(Duration::from_millis(500)).await;
    }

    println!(
        "Poll cycle done: sent {total} new item(s) in {:?}",
        started.elapsed()
    );
}

/// ---------- Main ----------

#[tokio::main]
async fn main() -> Result<()> {
    // --- Config & init ---
    let cfg = Config::from_env()?;

    // Telegram bot
    let bot = teloxide::Bot::new(&cfg.token);
    let chat_id = ChatId(cfg.chat_id);

    // HTTP client with timeouts and UA
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .tcp_keepalive(Duration::from_secs(30))
        .user_agent("My RSS Fetcher")
        .build()?;

    // Load state
    let mut state = State::load(&cfg.state_file).context("load_state")?;

    // Run once immediately
    println!(
        "Starting: {} feed(s), dedup_limit={}, poll_every={} min",
        cfg.feeds.len(),
        cfg.dedup_limit,
        cfg.poll_every_minutes
    );
    run_once(
        &client,
        &bot,
        chat_id,
        &mut state,
        &cfg.state_file,
        &cfg.feeds,
        cfg.dedup_limit,
    )
    .await;

    // Cron-like loop with graceful shutdown
    let mut ticker = tokio::time::interval(Duration::from_secs(60 * cfg.poll_every_minutes));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("Shutting down...");
                break;
            }
            _ = ticker.tick() => {
                run_once(&client, &bot, chat_id, &mut state, &cfg.state_file, &cfg.feeds, cfg.dedup_limit).await;
            }
        }
    }

    Ok(())
}