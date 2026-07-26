//! Live Stability API probe — `#[ignore]`d, following the precedent of the live
//! provider probes in `tests/consult.rs`: never run by `cargo test`, run by hand with
//! `--ignored` once a real credential is present. Spends real Stability credits (3 for
//! a `core` generation) — this is deliberate and the only test in this module that
//! touches the network.

use std::time::Duration;

use kaibo::stability::{
    resolve_key, GenerateRoute, MediaType, Operation, StabilityClient, StabilityRequest,
    StabilityResponse,
};

#[tokio::test]
#[ignore = "hits the Stability API (generate/core, 3 credits); run with --ignored and \
            ~/.stability-key.txt or STABILITY_API_KEY present"]
async fn generate_core_returns_a_real_png_with_a_seed() {
    let key = match resolve_key() {
        Ok(k) => k,
        Err(e) => panic!("no Stability credential for live test: {e}"),
    };
    let client = StabilityClient::new(key, kaibo::stability::STABILITY_API_BASE, Duration::from_secs(60))
        .expect("client construction");

    let request = StabilityRequest {
        prompt: "a small lighthouse on a rocky cliff, watercolor".to_string(),
        input_image: None,
        aspect_ratio: Some("1:1".to_string()),
        fields: vec![("output_format".to_string(), "png".to_string())],
    };

    let response = client
        .call(&Operation::Generate(GenerateRoute::Core), &request)
        .await
        .expect("a live generate/core call should succeed");

    // `Operation::Generate` always declares `Shape::Sync` (see `Operation::shape`), so a
    // live call must resolve to `Complete`, never `Deferred` — asserting that here keeps
    // this the one live check that the shape declaration matches reality.
    let image = match response {
        StabilityResponse::Complete(artifact) => artifact,
        StabilityResponse::Deferred(id) => {
            panic!("generate/core is declared Shape::Sync but returned a deferred job id {id:?}")
        }
    };

    eprintln!(
        "=== STABILITY LIVE: {} bytes, media_type={}, seed={:?} ===",
        image.bytes.len(),
        image.media_type.as_str(),
        image.seed
    );

    assert!(!image.bytes.is_empty(), "must return non-empty image bytes");
    // PNG magic bytes — confirms this is really an image, not e.g. a stray JSON body
    // that slipped past `handle_response`'s content-type check.
    assert_eq!(&image.bytes[..8], b"\x89PNG\r\n\x1a\n", "expected PNG magic bytes");
    assert_eq!(image.media_type, MediaType::ImagePng);
    assert!(
        image.seed.is_some(),
        "Stability should report the seed it used, for reproducibility"
    );
}
