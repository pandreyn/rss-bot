use super::*;
use std::env;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn test_dequote() {
    assert_eq!(dequote("'abc'"), "abc");
    assert_eq!(dequote("\"xyz\""), "xyz");
    assert_eq!(dequote("noquotes"), "noquotes");
}

#[test]
fn test_config_from_env() {
    // Set required env vars
    env::set_var("RSSBOT_TELEGRAM_TOKEN", "tokentest");
    env::set_var("RSSBOT_TELEGRAM_CHAT_ID", "12345");
    env::set_var("RSSBOT_FEEDS", "https://example.com/feed.xml");

    let cfg = Config::from_env().expect("Config should parse from env");
    assert_eq!(cfg.token, "tokentest");
    assert_eq!(cfg.chat_id, 12345);
    assert!(!cfg.feeds.is_empty());

    // Clean up
    env::remove_var("RSSBOT_TELEGRAM_TOKEN");
    env::remove_var("RSSBOT_TELEGRAM_CHAT_ID");
    env::remove_var("RSSBOT_FEEDS");
}

#[test]
fn test_state_mark_and_dedup_limit() {
    let mut state = State::default();
    let url = Url::parse("https://example.com/feed.xml").unwrap();
    state.ensure_feed(&url);

    // add items beyond dedup limit and ensure oldest dropped
    let dedup_limit = 3usize;
    state.mark_sent(&url, "id1".to_string(), dedup_limit);
    state.mark_sent(&url, "id2".to_string(), dedup_limit);
    state.mark_sent(&url, "id3".to_string(), dedup_limit);
    state.mark_sent(&url, "id4".to_string(), dedup_limit);

    let dq = state.seen_per_feed.get(url.as_str()).unwrap();
    assert_eq!(dq.len(), dedup_limit);
    assert!(!dq.contains(&"id1".to_string()));
    assert!(dq.contains(&"id4".to_string()));
}

#[test]
fn test_save_and_load_state_atomic() {
    let mut state = State::default();
    let url = Url::parse("https://example.com/feed.xml").unwrap();
    state.ensure_feed(&url);
    state.mark_sent(&url, "abc".to_string(), 10);

    // temp path
    let mut path = env::temp_dir();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
    path.push(format!("rssbot-test-state-{}.json", now));

    // ensure no file exists
    let _ = fs::remove_file(&path);

    save_state_atomic(&path, &state).expect("save should succeed");

    let loaded = State::load(&path).expect("load should succeed");
    let dq = loaded.seen_per_feed.get(url.as_str()).unwrap();
    assert!(dq.contains(&"abc".to_string()));

    let _ = fs::remove_file(&path);
}