//! Live image-generation probe — `#[ignore]`d, needs a real OpenAI-compatible image
//! endpoint (the local lemonade `sd-cpp` server with a turbo SD model pulled).
//!
//! **Load-bearing, not optional.** The offline tests (`src/image_gen.rs`,
//! `src/generate_image.rs`) drive a scripted backend that returns fixed bytes — so
//! they prove the wiring (sniff, cap, MCP content, openai-only gate) but never put a
//! real request on the wire. A malformed `/v1/images/generations` body, a response
//! shape rig can't decode, or a model the server doesn't have would all pass offline
//! and only fail here. This is the same lesson view_image's live VLM probe teaches:
//! offline-green ≠ live-works.
//!
//! Run it (lemonade up, model pulled):
//!   lemonade pull SDXL-Turbo        # or set KAIBO_IMAGE_MODEL to one you have
//!   cargo test --test image_gen_live -- --ignored --nocapture
//!
//! Overrides: `KAIBO_IMAGE_BASE_URL` (default the local lemonade endpoint),
//! `KAIBO_IMAGE_MODEL` (default `SDXL-Turbo`).

use std::time::Duration;

use kaibo::config::{Backend, ModelSlot};
use kaibo::credentials::ProviderKind;
use kaibo::generate_image::generate;
use kaibo::image_gen::RigOpenaiImageGen;

#[tokio::test]
#[ignore = "needs a live openai-compatible image endpoint (local lemonade sd-cpp + a pulled SD model)"]
async fn local_vlm_generates_a_real_image() {
    let base_url = std::env::var("KAIBO_IMAGE_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:13305/api/v1".to_string());
    let model = std::env::var("KAIBO_IMAGE_MODEL").unwrap_or_else(|_| "SDXL-Turbo".to_string());
    eprintln!("generating against {base_url} with model {model}");

    let backend = Backend {
        name: "lemonade".into(),
        kind: ProviderKind::Openai,
        base_url: Some(base_url),
        api_key_env: None,
        api_key_file: None,
        key_optional: true,
        request_timeout: Duration::from_secs(120),
    };
    let slot = ModelSlot::bare("lemonade", model);
    let generator = RigOpenaiImageGen::from_slot(&backend, &slot).expect("openai image generator");

    // A small size keeps a turbo model fast and the bytes under the inline cap.
    let image = generate(
        &generator,
        "a single red circle centered on a white background",
        (512, 512),
    )
    .await
    .expect("the live endpoint should return a decodable image");

    eprintln!("got {} bytes, sniffed as {}", image.bytes.len(), image.mime);
    assert!(
        !image.bytes.is_empty(),
        "a real generation must return image bytes"
    );
    // `generate` already sniffed the MIME (png/jpeg/gif/webp) and enforced the cap;
    // reaching here means a genuine image came back over the real wire.
    assert!(
        ["image/png", "image/jpeg", "image/webp", "image/gif"].contains(&image.mime),
        "unexpected mime {}",
        image.mime
    );
}
