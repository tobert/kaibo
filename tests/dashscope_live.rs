//! Live DashScope probe — `#[ignore]`d, following `tests/openai_images_live.rs` and
//! `tests/stability_live.rs`: never run by `cargo test`, run by hand with `--ignored`
//! once a real credential is present. Spends real money (one `wan2.6-t2i` image).
//!
//! This is the only witness to the whole path, because the artifact fetch takes
//! `https` and so cannot run against the offline socket in
//! `tests/dashscope_transport.rs`: generation → presigned link → fetched bytes →
//! `MediaArtifact` with a mime the CAS can store.
//!
//! A dedicated-endpoint subscription has its own host, so `KAIBO_DASHSCOPE_BASE_URL`
//! overrides the shared international default. Without it the probe still runs, just
//! against `dashscope-intl.aliyuncs.com`.

use std::path::PathBuf;
use std::time::Duration;

use kaibo::dashscope::{DashScopeClient, DashScopeImageModel, DASHSCOPE_API_BASE};
use kaibo::media::{FieldValue, MediaModel, MediaOutcome, MediaRequest};

/// The key sources the `dashscope` kind seeds: `DASHSCOPE_API_KEY`, then
/// `~/.dashscope-key`, env winning.
fn dashscope_key() -> anyhow::Result<String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set; cannot locate the key-file"))?;
    let env_value = std::env::var(kaibo::dashscope::DASHSCOPE_KEY_ENV_VAR).ok();
    kaibo::credentials::resolve(
        env_value.as_deref(),
        &home.join(kaibo::dashscope::DASHSCOPE_KEY_FILE_NAME),
    )
}

#[tokio::test]
#[ignore = "hits DashScope (wan2.6-t2i, one image, real money); run with --ignored and \
            ~/.dashscope-key or DASHSCOPE_API_KEY present, optionally \
            KAIBO_DASHSCOPE_BASE_URL for a dedicated endpoint"]
async fn wan_t2i_returns_real_image_bytes_through_a_fetched_link() {
    let key = match dashscope_key() {
        Ok(k) => k,
        Err(e) => panic!("no DashScope credential for live test: {e}"),
    };
    let base_url = std::env::var("KAIBO_DASHSCOPE_BASE_URL")
        .unwrap_or_else(|_| DASHSCOPE_API_BASE.to_string());
    let client =
        DashScopeClient::new(key, base_url, Duration::from_secs(300)).expect("client construction");
    let model = DashScopeImageModel::from_parts(&client, "wan2.6-t2i");

    let request = MediaRequest {
        prompt: "a small lighthouse on a rocky cliff, watercolor".to_string(),
        // One image at the smallest legal area — DashScope's total-pixel floor is
        // 589824, so 768*768 is the cheapest shape that is not a 400.
        fields: vec![
            (
                "n".to_string(),
                FieldValue::Num(serde_json::Number::from(1)),
            ),
            ("size".to_string(), FieldValue::Str("768*768".to_string())),
        ],
        inputs: Vec::new(),
        op: None,
    };
    let outcome = model.generate(&request).await.expect("live generation");
    let MediaOutcome::Complete { artifacts, .. } = outcome else {
        panic!("this route is synchronous, so the outcome is always Complete");
    };

    assert_eq!(artifacts.len(), 1, "n = 1 asked for exactly one image");
    let artifact = &artifacts[0];
    assert!(
        artifact.bytes.len() > 1024,
        "a real image is more than a kilobyte, got {} bytes",
        artifact.bytes.len()
    );
    // The mime came from the object store's own Content-Type, and it must be one the
    // CAS can name an extension for — that pairing is the whole point of refusing to
    // guess it.
    let ext = kaibo::cas::Extension::from_mime(&artifact.mime).unwrap_or_else(|| {
        panic!(
            "the CAS must know the fetched mime, got {:?}",
            artifact.mime
        )
    });
    assert!(
        ext.is_image(),
        "a generated artifact stores as an image, got {:?}",
        artifact.mime
    );
    // Prove the bytes really are what the mime claims, rather than an error page
    // served with an image content-type.
    let magic_ok = match ext {
        kaibo::cas::Extension::Png => artifact.bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        kaibo::cas::Extension::Jpeg => artifact.bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        kaibo::cas::Extension::Webp => {
            artifact.bytes.starts_with(b"RIFF") && artifact.bytes[8..12].starts_with(b"WEBP")
        }
        kaibo::cas::Extension::Gif => artifact.bytes.starts_with(b"GIF8"),
        other => panic!("unexpected image extension {other:?}"),
    };
    assert!(
        magic_ok,
        "the fetched bytes must actually be {:?}, not a page served under that type",
        artifact.mime
    );
}
