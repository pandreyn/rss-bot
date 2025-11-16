use anyhow::{anyhow, Context, Result};
use feed_rs::model::{Entry, Feed};
use feed_rs::parser;
use reqwest::{Client, StatusCode, Url};
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
use tracing::{debug, error, info, warn};
use tracing_subscriber::{fmt, EnvFilter};

/// ------------------------- Entry utilities -------------------------

fn entry_id(entry: &Entry) -> String {
    if let Some(id) = (!entry.id.is_empty()).then(|| entry.id.clone()) {
        return format!("guid:{id}");
    }
    // Prefer link + published if available (published not used; keep to make ID more stable if needed)
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
        return format!("titlepub:{}{}", title.content, published.timestamp());
    }
    // Fallback: hash of several fields
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
    if let (Some(summary), Some(_)) = (entry.summary.as_ref(), entry.published) {
        hasher.update("\n");
        hasher.update(summary.content.as_str());
    }
    if let Some(content) = entry.content.as_ref().and_then(|c| c.body.as_deref()) {
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
    // Prefer "alternate" link
    if let Some(href) = entry
        .links
        .iter()
        .find(|l| l.rel.as_deref().unwrap_or("alternate") == "alternate")
        .map(|l| l.href.clone())
    {
        return href;
    }
    // Otherwise, first link if present
    match entry.links.get(0).and_then(|l| Some(l.href.clone())) {
        Some(href) => href,
        None => String::new(),
    }
}

/// ------------------------- HTTP fetch -------------------------

async fn fetch_feed(client: &Client, url: &Url) -> Result<Option<Feed>> {
    let url_str = url.as_str();
    let resp = match client.get(url.clone()).send().await {
        Ok(r) => r,
        Err(e) => {
            error!(%url_str, error = %e, "HTTP GET failed to start");
            return Err(anyhow!("GET {:?}: {}", url_str, e));
        }
    };
    if resp.status() == StatusCode::NOT_MODIFIED {
        debug!(%url_str, "not modified");
        return Ok(None);
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        error!(%url_str, %status, body = body.as_str(), "non-success HTTP status");
        return Err(anyhow!("{} -> HTTP {} body={}", url_str, status, body));
    }
    let bytes = resp.bytes().await?;
    let cursor = Cursor::new(bytes);
    let feed =
        parser::parse(cursor).with_context(|| format!("parse feed {:?}", url_str))?;
    Ok(Some(feed))
}

/// ------------------------- Persistent state (dedup) -------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// feed_url -> queue of seen item IDs (oldest at front)
    seen_per_feed: HashMap<String, VecDeque<String>>,
}

impl State {
    fn load(path: &Path) -> Result<Self> {
        if path.exists() {
            let data = fs::read(path).with_context(|| format!("read {}", path.display()))?;
            let s: Self = serde_json::from_slice(&data).context("parse state JSON")?;
            Ok(s)
        } else {
            Ok(Default::default())
        }
    }

    fn ensure_feed(&mut self, url: &Url) {
        self.seen_per_feed.entry(url.as_str().to_string()).or_default();
    }

    fn seen(&self, url: &Url, id: &str) -> bool {
        self.seen_per_feed
            .get(url.as_str())
            .map_or(false, |dq| dq.contains(&id.to_string()))
    }

    fn mark_sent(&mut self, url: &Url, id: String, dedup_limit: usize) {
        let dq = self
            .seen_per_feed
            .entry(url.as_str().to_string())
            .or_default();
        if dq.contains(&id) {
            return;
        }
        dq.push_back(id);
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
    let json = serde_json::to_vec_pretty(state).context("serialize state JSON")?;
    fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| {
        format!("atomic rename {} -> {}", tmp.display(), path.display())
    })?;
    Ok(())
}

/// ------------------------- Runtime configuration -------------------------

#[derive(Debug)]
struct Config {
    token: String,
    chat_id: i64,
    feeds: Vec<Url>,
    dedup_limit: usize,
    poll_every_minutes: u64,
    state_file: PathBuf,
}

fn dequote(s: &str) -> &str {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

impl Config {
    fn from_env() -> Result<Self> {
        let token = env::var("RSSBOT_TELEGRAM_TOKEN")
            .context("TELEGRAM_TOKEN env var is required")?;
        let chat_id: i64 = env::var("RSSBOT_TELEGRAM_CHAT_ID")
            .context("TELEGRAM_CHAT_ID env var is required")?
            .parse()
            .context("TELEGRAM_CHAT_ID must be a valid i64")?;

        let feeds_raw = env::var("RSSBOT_FEEDS").context("FEEDS env var is required")?;
        let mut feeds = Vec::new();
        for raw in feeds_raw.split(|c: char| c == ',' || c == '\n' || c == ';' || c.is_whitespace())
        {
            let cleaned = dequote(raw).trim();
            if cleaned.is_empty() {
                continue;
            }
            let url = Url::parse(cleaned)
                .with_context(|| format!("Invalid URL in FEEDS: {:?}", cleaned))?;
            match url.scheme() {
                "http" | "https" => {}
                other => anyhow::bail!("Unsupported URL scheme {:?} in FEEDS: {:?}", other, cleaned),
            }
            feeds.push(url);
        }
        if feeds.is_empty() {
            anyhow::bail!("FEEDS must contain at least one valid absolute URL");
        }

        let dedup_limit: usize = env::var("RSSBOT_DEDUP_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200);

        let poll_every_minutes: u64 = env::var("RSSBOT_POLL_EVERY_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);

        let state_file = env::var("RSSBOT_STATE_FILE")
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

/// ------------------------- Feed processing -------------------------

async fn process_feed(
    client: &Client,
    bot: &teloxide::Bot,
    chat_id: ChatId,
    state: &mut State,
    state_path: &Path,
    feed_url: &Url,
    dedup_limit: usize,
) -> Result<(usize, String)> {
    let feed_opt = fetch_feed(client, feed_url).await?;
    // If not modified or no feed, return (0, url) so caller still has a name
    let Some(feed) = feed_opt else {
        return Ok((0, feed_url.as_str().to_string()));
    };

    let feed_tag = feed
        .title
        .as_ref()
        .map(|t| t.content.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| feed_url.as_str().to_string());

    // Send oldest first
    let mut sent_count = 0usize;
    for entry in feed.entries.iter().rev() {
        let id = entry_id(entry);
        if state.seen(feed_url, &id) {
            debug!(feed = %feed_url, %id, "already seen");
            continue;
        }

        let title = entry_title(entry);
        let link = entry_link(entry);
        let text = format!("[{feed_tag}]\n{title}\n{link}");

        if let Err(e) = bot.send_message(chat_id, text).await {
            error!(feed = %feed_url, error = %e, "telegram send failed");
            continue;
        }

        sent_count += 1;

        state.mark_sent(feed_url, id, dedup_limit);
        if let Err(e) = save_state_atomic(state_path, state) {
            warn!(error = %e, "failed to persist state (continuing)");
        }

        time::sleep(Duration::from_millis(100)).await;
    }
    Ok((sent_count, feed_tag))
}

async fn run_once(
    client: &Client,
    bot: &teloxide::Bot,
    chat_id: ChatId,
    state: &mut State,
    state_path: &Path,
    feeds: &[Url],
    dedup_limit: usize,
) -> Result<()> {
    let started = std::time::Instant::now();
    let mut total = 0usize;
    let mut per_feed: Vec<String> = Vec::new();

    for url in feeds {
        state.ensure_feed(url);

        match process_feed(client, bot, chat_id, state, state_path, url, dedup_limit).await {
            Ok((n, feed_name)) => {
                total += n;
                per_feed.push(format!("{}:{}", feed_name, n));
            }
            Err(e) => error!(feed = %url, error = %e, "feed error"),
        }

        time::sleep(Duration::from_millis(500)).await;
    }

    info!(
        sent = total,
        took = ?started.elapsed(),
        breakdown = %per_feed.join(", "),
        "poll cycle done"
    );
    Ok(())
}

/// ------------------------- Main -------------------------
#[tokio::main]
async fn main() -> Result<()> {
    // --- Logging ---
    // Human logs by default; set RUST_LOG_FORMAT=json for JSON lines.
    let json = env::var("RSSBOT_RUST_LOG_FORMAT").ok().as_deref() == Some("json");
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if json {
        fmt().with_env_filter(filter).json().with_target(false).init();
    } else {
        fmt().with_env_filter(filter).with_target(false).init();
    }

    // --- Config ---
    let cfg = Config::from_env()?;
    info!(
        feeds = cfg.feeds.len(),
        dedup_limit = cfg.dedup_limit,
        poll_every_min = cfg.poll_every_minutes,
        state_file = %cfg.state_file.display(),
        "startup"
    );

    // Telegram bot
    let bot = teloxide::Bot::new(&cfg.token);
    let chat_id = ChatId(cfg.chat_id);

    // HTTP client (rustls via Cargo.toml features)
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .tcp_keepalive(Duration::from_secs(30))
        .user_agent("rss-bot/0.1 (+https://github.com/pandreyn/rss-bot)")
        .build()?;

    // Load state
    let mut state = State::load(&cfg.state_file).context("load_state")?;

    // Run once immediately
    run_once(
        &client,
        &bot,
        chat_id,
        &mut state,
        &cfg.state_file,
        &cfg.feeds,
        cfg.dedup_limit,
    )
    .await?;

    // Cron-like loop with graceful shutdown
    let mut ticker = time::interval(Duration::from_secs(60 * cfg.poll_every_minutes));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received ctrl-c, shutting down");
                break;
            }
            _ = ticker.tick() => {
                if let Err(e) = run_once(&client, &bot, chat_id, &mut state, &cfg.state_file, &cfg.feeds, cfg.dedup_limit).await {
                    error!(error = %e, "poll cycle failed");
                }
            }
        }
    }

    Ok(())
}