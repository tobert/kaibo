//! Live OpenAI Images API probe — `#[ignore]`d, following the precedent of
//! `tests/stability_live.rs` and the live provider probes in `tests/consult.rs`:
//! never run by `cargo test`, run by hand with `--ignored` once a real credential
//! is present. Spends real OpenAI credits (one gpt-image-1 generation at the
//! cheapest quality/size) — deliberate, and the only test in this module that
//! touches the network.

use std::path::PathBuf;
use std::time::Duration;

use kaibo::media::MediaRequest;
use kaibo::openai_images::OpenAiImagesClient;

/// The same key sources the `openai-images` kind seeds (`OPENAI_API_KEY` /
/// `~/.openai-key`, env wins) — resolved directly because this probe targets the
/// hosted API, where a key is mandatory even though the kind is `key_optional`.
fn hosted_key() -> anyhow::Result<String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set; cannot locate the key-file"))?;
    let env_value = std::env::var("OPENAI_API_KEY").ok();
    kaibo::credentials::resolve(env_value.as_deref(), &home.join(".openai-key"))
}

#[tokio::test]
#[ignore = "hits the OpenAI Images API (gpt-image-1, low quality, real money); run with \
            --ignored and ~/.openai-key or OPENAI_API_KEY present"]
async fn gpt_image_1_returns_real_png_bytes() {
    let key = match hosted_key() {
        Ok(k) => k,
        Err(e) => panic!("no OpenAI credential for live test: {e}"),
    };
    let client = OpenAiImagesClient::new(
        key,
        kaibo::credentials::HOSTED_OPENAI_BASE_URL,
        Duration::from_secs(120),
    )
    .expect("client construction");

    let request = MediaRequest {
        prompt: "a small lighthouse on a rocky cliff, watercolor".to_string(),
        input_image: None,
        // The cheapest hosted shape; output_format png is gpt-image-1's default,
        // stated explicitly so the mime derivation is exercised end to end.
        fields: vec![
            ("size".to_string(), "1024x1024".into()),
            ("quality".to_string(), "low".into()),
            ("output_format".to_string(), "png".into()),
        ],
    };

    let artifacts = client
        .generate("gpt-image-1", &request)
        .await
        .expect("a live gpt-image-1 call should succeed");

    eprintln!(
        "=== OPENAI IMAGES LIVE: {} artifact(s), first {} bytes, mime {} ===",
        artifacts.len(),
        artifacts[0].bytes.len(),
        artifacts[0].mime
    );

    assert_eq!(artifacts.len(), 1, "no `n` field, so exactly one artifact");
    let image = &artifacts[0];
    assert!(!image.bytes.is_empty(), "must return non-empty image bytes");
    // PNG magic bytes — confirms the b64 envelope really decoded to an image.
    assert_eq!(
        &image.bytes[..8],
        b"\x89PNG\r\n\x1a\n",
        "expected PNG magic bytes"
    );
    assert_eq!(image.mime, "image/png");
    assert_eq!(image.seed, None, "the Images API reports no seed");
}
