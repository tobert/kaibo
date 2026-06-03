//! Live end-to-end explorer test. Hits the Anthropic API, so it is `#[ignore]`d
//! by default. Run it deliberately:
//!
//! ```sh
//! cargo test --test explorer_live -- --ignored
//! ```
//!
//! It needs an Anthropic credential (env `ANTHROPIC_API_KEY` or `~/.anthropic-key.txt`).
//! It points the explorer at kaibo's own source tree and asks a question whose
//! answer lives in `src/sandbox.rs`.

use kaibo::credentials::{load, Provider};
use kaibo::explorer::{explore, ExploreConfig};

#[tokio::test]
#[ignore = "hits the Anthropic API; run with --ignored and a configured key"]
async fn explorer_answers_from_the_real_tree() {
    let key = match load(Provider::Anthropic) {
        Ok(k) => k,
        Err(e) => panic!("no Anthropic credential for live test: {e}"),
    };

    let root = env!("CARGO_MANIFEST_DIR");
    let cfg = ExploreConfig::default();

    let report = explore(
        "Which source file enforces the read-only sandbox, and name one builtin it blocks?",
        root,
        &key,
        &cfg,
    )
    .await
    .expect("explore should succeed");

    eprintln!("--- explorer report ---\n{report}\n-----------------------");
    let lower = report.to_lowercase();
    assert!(
        lower.contains("sandbox"),
        "report should reference the sandbox module, got: {report}"
    );
}
