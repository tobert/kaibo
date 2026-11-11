//! A live probe of DeepSeek's Search and Fetch APIs through `WebClient`. Ignored by
//! default: it makes one search and one fetch ($0.75 per 1,000 calls each) and needs the
//! key in `~/.deepseek-key` (or `DEEPSEEK_API_KEY`). Run with
//! `cargo test --test web_live -- --ignored`.

use std::time::Duration;

use kaibo::web::{SearchRequest, WebClient};

fn key() -> String {
    if let Ok(k) = std::env::var("DEEPSEEK_API_KEY") {
        return k;
    }
    let home = std::env::var("HOME").expect("HOME");
    std::fs::read_to_string(format!("{home}/.deepseek-key"))
        .expect("DEEPSEEK_API_KEY or ~/.deepseek-key")
        .trim()
        .to_string()
}

#[tokio::test]
#[ignore = "live: one search and one fetch against DeepSeek"]
async fn deepseek_search_then_fetch() {
    kaibo::tls::ensure_crypto_provider();
    let client = WebClient::deepseek(key(), Duration::from_secs(60)).unwrap();
    let req = SearchRequest::new("rig-core rust crate docs.rs", 3, None).unwrap();
    let results = client.search(&req).await.expect("search");
    assert!(!results.is_empty(), "a common query returns results");
    for r in &results {
        assert!(r.url.starts_with("http"), "{r:?}");
        eprintln!(
            "{} {:?} {:?} {}",
            r.url, r.authority, r.rerank_score, r.title
        );
    }
    let page = client
        .fetch("https://api-docs.deepseek.com/")
        .await
        .expect("fetch");
    assert!(!page.content.trim().is_empty(), "{page:?}");
    eprintln!("fetched {:?}, {} chars", page.title, page.content.len());
}
