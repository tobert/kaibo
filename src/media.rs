//! The media-generation seam: kaibo's counterpart of rig's `CompletionModel` for
//! backends that produce artifacts instead of text.
//!
//! # Why a kaibo trait, when rig ships `ImageGenerationModel`
//!
//! rig's trait is one synchronous text-to-image call (`prompt`/`width`/`height`), with
//! nowhere to hand a deferred job id — and Stability's v2beta family is bigger than
//! that (deferred upscale/audio/3D operations whose POST returns only an id; see
//! `src/stability.rs`'s module doc). kaibo's tool lane needs the deferred half as a
//! first-class outcome: a sync operation answers in-call, a deferred one becomes a
//! kaibo job the caller polls. So [`MediaModel`] carries both, the way
//! [`crate::batch::BatchProvider`] carries submit *and* poll.
//!
//! The vocabulary here is deliberately provider-neutral (a [`MediaArtifact`] is bytes,
//! a mime string, and a seed) — providers translate their native shapes at their own
//! impl, exactly as rig providers translate into rig's completion types. The one live
//! implementation today is Stability's (`src/stability.rs`).
//!
//! # The construction point
//!
//! [`MediaArm::from_slot`] is the single place a media backend becomes callable — the
//! media mirror of `Arm::from_slot` (`consult/engine.rs`), and the enforcement seam
//! for the completion/media split: it accepts only [`ProviderClass::Media`] kinds and
//! bails loudly on a completion backend, the mirror image of `Arm::from_slot`'s
//! media bail. Config validation (`config.rs`) refuses the mismatched pairings at
//! load, so both bails are belts to those braces.
//!
//! Nothing here touches the filesystem: a [`MediaArtifact`]'s bytes live in memory
//! until a caller hands them to the media CAS (`src/cas.rs`), which is one of the two
//! blessed write surfaces and does its own containment checks.

use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;

use crate::config::{Backend, ModelSlot};
use crate::credentials::{MediaKind, ProviderClass};

/// One generation request, provider-neutral. The prompt is the portable half; every
/// provider-native knob (seed, aspect ratio, negative prompt, output format, ...)
/// rides `fields` untyped — kaibo keeps no allowlist, the provider answers for its own
/// knobs, the same passthrough posture `additional_params` takes on the completion
/// side. (`prompt` and `model` never ride `fields`: the tool layer reserves both, so
/// the recorded provenance always describes the request that ran.)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaRequest {
    pub prompt: String,
    /// `(name, value)` provider-native fields: **uniquely named**, in the order the
    /// caller gave them (kept as a `Vec` because a provider may care about order —
    /// the MCP face is a map, so duplicate names cannot arrive from a tool call). A
    /// provider impl may seed its own defaults (Stability seeds `output_format`) and
    /// lets a caller field of the same name replace the seeded default — that merge
    /// is the provider's business, not a contract of this struct.
    pub fields: Vec<(String, String)>,
    /// Bytes of an input image, for the operations that take one (edit / upscale /
    /// image-to-image). `None` for text-to-image. Carried on the request now so the
    /// trait does not churn when those operations land.
    pub input_image: Option<Vec<u8>>,
}

/// One generated artifact, provider-neutral: the bytes, the wire content type in the
/// provider's own spelling (`"image/png"`), and the provider-reported seed when one
/// exists — the reproduction handle the CAS provenance sidecar records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaArtifact {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub seed: Option<String>,
}

/// The id of one deferred generation, as the provider spelled it. Opaque: it
/// round-trips through kaibo's job store as a string and only the provider's own
/// poll route interprets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaJobId(pub String);

/// One `generate` call's outcome. Sync operations resolve straight to `Complete`;
/// a deferred operation's POST resolves to `Deferred`, and the artifacts come from
/// [`MediaModel::poll`] later. Which shape a given operation has is the *provider's*
/// declared fact (see `stability::Operation::shape`), never sniffed from a response.
///
/// `Complete` carries a **list**: many image models return several images per call
/// (Amy's call, 2026-08-03), so one generation is one-to-many artifacts. Each artifact
/// gets its own CAS digest and its own provenance sidecar downstream — a result is a
/// list of digests, never a single blessed one. A provider that returns exactly one
/// (Stability's ops today) hands back a one-element list; an *empty* list from a
/// provider is that provider's bug to refuse at its own impl, not a shape this enum
/// blesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaOutcome {
    Complete(Vec<MediaArtifact>),
    Deferred(MediaJobId),
}

/// One poll's outcome: still running, or done. A poll is never itself deferred
/// again — `Pending` means "ask again later", with the cadence owned by the caller
/// (the tool lane brings its own deadline/backoff; this seam never sleeps).
/// `Complete` is a list for the same one-to-many reason as
/// [`MediaOutcome::Complete`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaPollOutcome {
    Pending,
    Complete(Vec<MediaArtifact>),
}

/// A media-generation model: one generate call plus one poll, both provider-neutral.
/// Object-safe (`Arc<dyn MediaModel>`) so the tool lane and tests dispatch over the
/// same seam — a scripted impl drives the lane offline, mirroring how
/// [`crate::batch::BatchProvider`] is exercised against `ScriptedBatch`.
#[async_trait]
pub trait MediaModel: Send + Sync {
    /// Run one generation request against the model this instance was built for.
    async fn generate(&self, request: &MediaRequest) -> Result<MediaOutcome>;

    /// Collect one deferred job's result. One shot, no retry loop — the caller owns
    /// the cadence.
    async fn poll(&self, job: &MediaJobId) -> Result<MediaPollOutcome>;
}

/// A staffed media slot, ready to call: the model behind an `image = "backend/id"`
/// cast slot. The media mirror of the completion `Arm`.
#[derive(Clone)]
pub struct MediaArm {
    model: Arc<dyn MediaModel>,
    /// The slot's `"backend/model-id"` ref — provenance and error text.
    slot_ref: String,
}

impl std::fmt::Debug for MediaArm {
    /// Manual: `dyn MediaModel` carries no `Debug` bound, and the slot ref is the
    /// arm's whole identity anyway.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaArm")
            .field("slot_ref", &self.slot_ref)
            .finish_non_exhaustive()
    }
}

impl MediaArm {
    /// Wrap an already-built model — the seam tests inject a scripted [`MediaModel`]
    /// through, mirroring `Arm::new` on the completion side. Production goes through
    /// [`from_slot`](Self::from_slot).
    pub fn new(model: Arc<dyn MediaModel>, slot_ref: impl Into<String>) -> Self {
        Self {
            model,
            slot_ref: slot_ref.into(),
        }
    }

    /// The single live construction point: resolve a media cast slot into a callable
    /// arm. Only a [`ProviderClass::Media`] backend can staff one — a completion
    /// backend is refused here with the same load-guard framing `Arm::from_slot` uses
    /// for the mirror-image mistake.
    pub fn from_slot(backend: &Backend, slot: &ModelSlot) -> Result<Self> {
        match backend.kind.class() {
            ProviderClass::Media(MediaKind::Stability) => {
                let key = backend.resolve_key()?;
                let base_url = backend
                    .base_url
                    .clone()
                    .unwrap_or_else(|| crate::stability::STABILITY_API_BASE.to_string());
                let client =
                    crate::stability::StabilityClient::new(key, base_url, backend.request_timeout)?;
                let model = crate::stability::StabilityImageModel::from_parts(&client, &slot.id);
                Ok(Self::new(Arc::new(model), slot.qualified()))
            }
            ProviderClass::Media(MediaKind::OpenAiImages) => {
                // Key posture mirrors the `openai` completion kind: `resolve_key`
                // hands back the placeholder for a keyless local sd-server
                // (`key_optional`, the seeded default) and a real key otherwise.
                let key = backend.resolve_key()?;
                // Unset dials hosted OpenAI — a root through /v1; the client appends
                // its own /images/generations. A local sd-server sets base_url.
                let base_url = backend
                    .base_url
                    .clone()
                    .unwrap_or_else(|| crate::credentials::HOSTED_OPENAI_BASE_URL.to_string());
                let client = crate::openai_images::OpenAiImagesClient::new(
                    key,
                    base_url,
                    backend.request_timeout,
                )?;
                let model =
                    crate::openai_images::OpenAiImagesModel::from_parts(&client, &slot.id);
                Ok(Self::new(Arc::new(model), slot.qualified()))
            }
            ProviderClass::Wire(_) => bail!(
                "backend {:?} is kind `{}`, a completion wire — it cannot staff a media \
                 slot. Point the `image` slot at a media backend, and use this backend \
                 on `explorer`/`synth` instead",
                backend.name,
                backend.kind.canonical_name()
            ),
        }
    }

    /// The `"backend/model-id"` this arm was staffed from.
    pub fn slot_ref(&self) -> &str {
        &self.slot_ref
    }

    /// Run one generation request on this arm.
    pub async fn generate(&self, request: &MediaRequest) -> Result<MediaOutcome> {
        self.model.generate(request).await
    }

    /// Collect one deferred job's result.
    pub async fn poll(&self, job: &MediaJobId) -> Result<MediaPollOutcome> {
        self.model.poll(job).await
    }
}

/// How the generate lane turns a cast's `image` slot into a callable [`MediaArm`] —
/// the injection seam, mirroring [`crate::batch::BatchProviderFactory`]: the handler
/// holds an `Arc<dyn MediaArmFactory>`, production seeds [`LiveMediaArms`] (the real
/// [`MediaArm::from_slot`] path), and tests swap in a factory returning a scripted
/// [`MediaModel`] so the whole tool lane — CAS writes, job lane, rendering — runs
/// offline with no network.
pub trait MediaArmFactory: Send + Sync {
    fn build(&self, backend: &Backend, slot: &ModelSlot) -> Result<MediaArm>;
}

/// The real construction path: [`MediaArm::from_slot`], the single live point where a
/// media backend becomes callable.
pub struct LiveMediaArms;

impl MediaArmFactory for LiveMediaArms {
    fn build(&self, backend: &Backend, slot: &ModelSlot) -> Result<MediaArm> {
        MediaArm::from_slot(backend, slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted model: answers `generate` with a fixed outcome and `poll` with a
    /// pending-then-complete sequence — enough to prove the trait dispatches as an
    /// object and the arm forwards faithfully, artifact *lists* included.
    struct Scripted {
        artifacts: Vec<MediaArtifact>,
        polls: std::sync::Mutex<usize>,
    }

    #[async_trait]
    impl MediaModel for Scripted {
        async fn generate(&self, request: &MediaRequest) -> Result<MediaOutcome> {
            if request.prompt.is_empty() {
                bail!("empty prompt");
            }
            Ok(MediaOutcome::Deferred(MediaJobId("job-1".to_string())))
        }

        async fn poll(&self, job: &MediaJobId) -> Result<MediaPollOutcome> {
            assert_eq!(job.0, "job-1");
            let mut n = self.polls.lock().unwrap();
            *n += 1;
            if *n == 1 {
                Ok(MediaPollOutcome::Pending)
            } else {
                Ok(MediaPollOutcome::Complete(self.artifacts.clone()))
            }
        }
    }

    fn png_artifact() -> MediaArtifact {
        MediaArtifact {
            bytes: vec![0x89, b'P', b'N', b'G'],
            mime: "image/png".to_string(),
            seed: Some("42".to_string()),
        }
    }

    fn webp_artifact() -> MediaArtifact {
        MediaArtifact {
            bytes: b"RIFF....WEBP".to_vec(),
            mime: "image/webp".to_string(),
            seed: None,
        }
    }

    /// The deferred round trip through the trait object: generate hands back a job,
    /// the first poll is pending, the second completes with EVERY artifact intact and
    /// in order — the one-to-many shape (many image models return several images per
    /// call) the outcome enums exist to carry.
    #[tokio::test]
    async fn deferred_generate_then_poll_round_trip_with_multiple_artifacts() {
        let arm = MediaArm::new(
            Arc::new(Scripted {
                artifacts: vec![png_artifact(), webp_artifact()],
                polls: std::sync::Mutex::new(0),
            }),
            "sd/core",
        );
        assert_eq!(arm.slot_ref(), "sd/core");
        let outcome = arm
            .generate(&MediaRequest {
                prompt: "a lighthouse".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let MediaOutcome::Deferred(job) = outcome else {
            panic!("scripted generate is deferred, got {outcome:?}");
        };
        assert_eq!(arm.poll(&job).await.unwrap(), MediaPollOutcome::Pending);
        let done = arm.poll(&job).await.unwrap();
        assert_eq!(
            done,
            MediaPollOutcome::Complete(vec![png_artifact(), webp_artifact()])
        );
    }

    /// A completion backend cannot staff a media arm: the mirror image of
    /// `Arm::from_slot`'s media bail, and the message names both the mistake and
    /// the fix.
    #[test]
    fn from_slot_refuses_a_completion_backend() {
        let cfg = crate::config::Config::builtin();
        let backend = cfg.backends.get("anthropic").expect("built-in exists");
        let slot = ModelSlot::bare("anthropic", "claude-sonnet-4-6");
        let err = MediaArm::from_slot(backend, &slot).expect_err("wire kind must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("completion wire") && msg.contains("media backend"),
            "the error names the problem and the fix, got: {msg}"
        );
    }

    /// An openai-images backend staffs an image slot — and the keyless default
    /// (`key_optional` seeds true for this kind) resolves to the placeholder, so a
    /// local sd-server backend needs no credential at all. Construction only; no
    /// network.
    #[test]
    fn from_slot_staffs_an_openai_images_backend_keylessly() {
        let cfg = crate::config::Config::from_toml_str(
            r#"
            [backends.sdcpp]
            kind = "openai-images"
            base_url = "http://localhost:1234/v1"
            api_key_file = "/nonexistent-kaibo-test/openai"
            "#,
        )
        .expect("config parses");
        let backend = cfg.backends.get("sdcpp").expect("backend exists");
        let slot = ModelSlot::bare("sdcpp", "sd3.5-large");
        let arm = MediaArm::from_slot(backend, &slot).expect("an openai-images backend staffs");
        assert_eq!(arm.slot_ref(), "sdcpp/sd3.5-large");
    }

    /// The Stability arm still staffs — the sibling-kind regression guard for the
    /// `from_slot` match growing a second media arm.
    #[test]
    fn from_slot_still_staffs_a_stability_backend() {
        let cfg = crate::config::Config::from_toml_str(
            r#"
            [backends.sd]
            kind = "stability"
            key_optional = true
            "#,
        )
        .expect("config parses");
        let backend = cfg.backends.get("sd").expect("backend exists");
        let slot = ModelSlot::bare("sd", "core");
        let arm = MediaArm::from_slot(backend, &slot).expect("a stability backend staffs");
        assert_eq!(arm.slot_ref(), "sd/core");
    }
}
