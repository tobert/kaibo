//! Live BFL probe — `#[ignore]`d, following `tests/dashscope_live.rs` and
//! `tests/stability_live.rs`: never run by `cargo test`, run by hand with
//! `--ignored` once a real credential is present. Spends real money (one
//! `flux-dev` image — the cheap rung, deliberately, per `docs/bfl.md`'s starter
//! table).
//!
//! No `BFL_API_KEY` existed at the time this landed (`docs/bfl.md`'s status log), so
//! this is unexercised against the real API — it proves the offline path builds and
//! is trivially skippable, and is the first thing to run once a key exists. It is
//! the only witness to the whole path end to end: the artifact fetch takes `https`
//! and so cannot run against the offline sockets in `tests/bfl_transport.rs` —
//! generation → poll → signed link → fetched bytes → `MediaArtifact` with a mime the
//! CAS can store.

use std::path::PathBuf;
use std::time::Duration;

use kaibo::bfl::{BflClient, BflImageModel};
use kaibo::media::{MediaModel, MediaOutcome, MediaRequest};

/// The key sources the `bfl` kind seeds: `BFL_API_KEY`, then `~/.bfl-key`, env
/// winning.
fn bfl_key() -> anyhow::Result<String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set; cannot locate the key-file"))?;
    let env_value = std::env::var(kaibo::bfl::BFL_KEY_ENV_VAR).ok();
    kaibo::credentials::resolve(
        env_value.as_deref(),
        &home.join(kaibo::bfl::BFL_KEY_FILE_NAME),
    )
}

#[tokio::test]
#[ignore = "hits BFL (flux-dev, one image, real money); run with --ignored and \
            ~/.bfl-key or BFL_API_KEY present"]
async fn flux_dev_returns_real_image_bytes_through_a_polled_link() {
    let key = match bfl_key() {
        Ok(k) => k,
        Err(e) => panic!("no BFL credential for live test: {e}"),
    };
    let client = BflClient::new(key, kaibo::bfl::DEFAULT_BASE_URL, Duration::from_secs(300))
        .expect("client construction");
    let model = BflImageModel::from_parts(&client, "flux-dev");

    let request = MediaRequest {
        prompt: "a small lighthouse on a rocky cliff, watercolor".to_string(),
        ..Default::default()
    };
    let outcome = model.generate(&request).await.expect("live generation");
    let artifacts = match outcome {
        MediaOutcome::Complete { artifacts, note } => {
            if let Some(note) = note {
                eprintln!("kaibo: {note}");
            }
            artifacts
        }
        MediaOutcome::Deferred(job) => {
            eprintln!(
                "kaibo: still generating after the inline budget, polling job {}",
                job.0
            );
            loop {
                match model.poll(&job).await.expect("poll") {
                    kaibo::media::MediaPollOutcome::Complete(a) => break a,
                    kaibo::media::MediaPollOutcome::Pending => {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        }
    };

    assert_eq!(artifacts.len(), 1);
    let artifact = &artifacts[0];
    assert!(
        artifact.bytes.len() > 1024,
        "a real image is more than a kilobyte, got {} bytes",
        artifact.bytes.len()
    );
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
}
